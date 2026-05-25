// Phase 1 scaffold — see nestty-term/src/lib.rs and
// docs/macos-renderer-migration-plan.md §D3.
//
// Pointer ownership:
//   nestty_term_create -> NesttyHandle*       Rust-owned, free with nestty_term_destroy
//   nestty_term_snapshot -> NesttySnapshot*   Rust-owned, free with nestty_snapshot_destroy
//   *const NesttyRun from row_runs            Borrowed from snapshot, valid until snapshot_destroy
//   *const uint8_t   from row_utf8            Borrowed from snapshot, same lifetime
//   nestty_term_version() -> const char*      Static, no free

#ifndef NESTTY_TERM_H
#define NESTTY_TERM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct NesttyHandle NesttyHandle;
typedef struct NesttySnapshot NesttySnapshot;
typedef struct NesttyString NesttyString;

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
} NesttyRun;

// Flags bit layout — must match nestty_term::flags:
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
} NesttyCursor;

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
} NesttySelectionRange;

// Selection-start kind discriminator for nestty_term_selection_start.
// BLOCK is the rectangular (column-major) variant tied to
// Option+drag in the renderer (Terminal.app / iTerm2 native gesture; Cmd is reserved for URL click) — alacritty's SelectionType::Block.
// Snapshot reports `NesttySelectionRange.is_block = 1` so the renderer
// can paint a rectangle instead of the row-wrapped span.
#define NESTTY_SELECTION_SIMPLE   0
#define NESTTY_SELECTION_SEMANTIC 1
#define NESTTY_SELECTION_LINES    2
#define NESTTY_SELECTION_BLOCK    3

// Side discriminator (which side of the cell the click landed on).
#define NESTTY_SIDE_LEFT  0
#define NESTTY_SIDE_RIGHT 1

// --- Terminal lifecycle ---

NesttyHandle* nestty_term_create(uint16_t cols, uint16_t rows,
                                  const char* shell, const char* cwd);
void nestty_term_destroy(NesttyHandle* handle);

void nestty_term_input(NesttyHandle* handle, const uint8_t* bytes, size_t len);

/// Populate one entry of the OSC color-query palette. Index follows
/// `vte::ansi::NamedColor`: 0-15 ANSI (normal + bright), 16-255 256-color,
/// 256 foreground, 257 background, 258 cursor. Re-call for the same
/// index to overwrite on theme hot-reload. Indices never set make
/// `Event::ColorRequest` (OSC 4/10/11/12) a silent no-op — apps fall
/// back to defaults rather than getting a color we don't draw.
/// Returns 0 on success, -1 on NULL handle.
int nestty_term_set_palette_entry(NesttyHandle* handle,
                                   uint16_t index,
                                   uint8_t r, uint8_t g, uint8_t b);

void nestty_term_resize(NesttyHandle* handle, uint16_t cols, uint16_t rows);

// Returns true if the grid has any pending damage since the last call;
// always resets internal damage state. Intended for CADisplayLink-driven
// renderers to skip work when nothing changed.
bool nestty_term_take_damage(NesttyHandle* handle);

// --- Snapshot ---

NesttySnapshot* nestty_term_snapshot(NesttyHandle* handle);
void nestty_snapshot_destroy(NesttySnapshot* snap);

uint16_t nestty_snapshot_rows(const NesttySnapshot* snap);
uint16_t nestty_snapshot_cols(const NesttySnapshot* snap);

// Sets *out_runs to a borrowed pointer; returns the run count. Both
// the pointer and the underlying memory live until snapshot_destroy.
size_t nestty_snapshot_row_runs(const NesttySnapshot* snap, uint16_t row,
                                 const NesttyRun** out_runs);

// Borrowed pointer to the row's utf8 bytes; same lifetime.
const uint8_t* nestty_snapshot_row_utf8(const NesttySnapshot* snap, uint16_t row,
                                         size_t* out_len);

void nestty_snapshot_cursor(const NesttySnapshot* snap, NesttyCursor* out);
void nestty_snapshot_selection(const NesttySnapshot* snap, NesttySelectionRange* out);

// --- Selection control ---

// Begin a new selection at (row, col, side) with the given kind
// (NESTTY_SELECTION_*). Replaces any existing selection.
void nestty_term_selection_start(NesttyHandle* handle, uint16_t row, uint16_t col,
                                  uint8_t side, uint8_t kind);
void nestty_term_selection_update(NesttyHandle* handle, uint16_t row, uint16_t col, uint8_t side);
void nestty_term_selection_clear(NesttyHandle* handle);
void nestty_term_selection_all(NesttyHandle* handle);

// Heap-allocated UTF-8 copy of the current selection. NULL when
// nothing selected. Caller frees with nestty_string_destroy exactly
// once.
NesttyString* nestty_term_selection_string(NesttyHandle* handle);
const uint8_t* nestty_string_bytes(const NesttyString* s, size_t* out_len);
void nestty_string_destroy(NesttyString* s);

// Renderer policy queries.
bool nestty_term_mouse_mode_active(NesttyHandle* handle);
bool nestty_term_bracketed_paste_active(NesttyHandle* handle);

// Mouse-event encoding negotiated by the TUI (SGR / legacy / UTF8 /
// none). Returns 0 when no reporting mode is on; otherwise picks the
// matching mutually-exclusive encoding bit. Use to format the bytes
// for forwarded scroll-wheel / click / drag events.
#define NESTTY_MOUSE_ENC_NONE   0
#define NESTTY_MOUSE_ENC_LEGACY 1
#define NESTTY_MOUSE_ENC_SGR    2
#define NESTTY_MOUSE_ENC_UTF8   3
uint8_t nestty_term_mouse_encoding(NesttyHandle* handle);

// Highest mouse-reporting level the TUI has enabled. Tiers stack:
// MOTION ⊇ DRAG ⊇ CLICK. Renderer uses this to gate which AppKit
// mouse events should forward (press/release always at CLICK+,
// drag-while-button-held at DRAG+, bare motion at MOTION).
#define NESTTY_MOUSE_LEVEL_NONE   0
#define NESTTY_MOUSE_LEVEL_CLICK  1
#define NESTTY_MOUSE_LEVEL_DRAG   2
#define NESTTY_MOUSE_LEVEL_MOTION 3
uint8_t nestty_term_mouse_report_level(NesttyHandle* handle);

// Drain the most-recent pending OSC 52 clipboard-store request.
// Returns NULL when nothing pending. Caller frees with
// nestty_string_destroy and gates the system clipboard write on
// the user's [security] osc52 policy.
NesttyString* nestty_term_take_clipboard_request(NesttyHandle* handle);

// Scrollback navigation. `kind` selects the variant; `delta` is only
// consulted for NESTTY_SCROLL_DELTA (positive = older content scrolls
// in; negative = newer).
#define NESTTY_SCROLL_DELTA     0
#define NESTTY_SCROLL_PAGE_UP   1
#define NESTTY_SCROLL_PAGE_DOWN 2
#define NESTTY_SCROLL_TOP       3
#define NESTTY_SCROLL_BOTTOM    4
void nestty_term_scroll(NesttyHandle* handle, uint8_t kind, int32_t delta);

// OSC 8 hyperlink URI lookup. `hyperlink_id` is the run's 1-based
// index from the snapshot; 0 means "no hyperlink". URI bytes are
// borrowed from snapshot storage — copy before destroy.
uint32_t nestty_snapshot_hyperlink_count(const NesttySnapshot* snap);
const uint8_t* nestty_snapshot_hyperlink_uri(const NesttySnapshot* snap,
                                              uint32_t hyperlink_id,
                                              size_t* out_len);

// Static string, no free required.
const char* nestty_term_version(void);

#endif
