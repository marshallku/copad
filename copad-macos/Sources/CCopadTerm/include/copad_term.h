// Rust side: copad-term/src/lib.rs.
//
// Pointer ownership:
//   copad_term_create -> CopadHandle*       free with copad_term_destroy
//   copad_term_snapshot -> CopadSnapshot*   free with copad_snapshot_destroy
//   *const CopadRun from row_runs, *const uint8_t from row_utf8: borrowed from the snapshot,
//   valid until snapshot_destroy
//   copad_term_version() -> const char*     static, no free

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
    uint32_t hyperlink_id; // 0 = none; opaque key into a separate hyperlink table
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

// Active selection bounds; end_row / end_col are inclusive (alacritty's SelectionRange).
// Meaningful only when `present == 1`.
typedef struct {
    uint16_t start_row;
    uint16_t start_col;
    uint16_t end_row;
    uint16_t end_col;
    uint8_t  is_block;
    uint8_t  present;
    uint16_t reserved;
} CopadSelectionRange;

// Selection-start kind for copad_term_selection_start. BLOCK is the rectangular variant tied to
// Option+drag (Cmd is reserved for URL click); the snapshot then reports `is_block = 1`.
#define COPAD_SELECTION_SIMPLE   0
#define COPAD_SELECTION_SEMANTIC 1
#define COPAD_SELECTION_LINES    2
#define COPAD_SELECTION_BLOCK    3

// Side discriminator (which side of the cell the click landed on).
#define COPAD_SIDE_LEFT  0
#define COPAD_SIDE_RIGHT 1

/// `panel_id` may be NULL; when set it is exported to the child shell as `COPAD_PANEL_ID`, so the
/// copad-cwd shell hook can target the panel. `socket_path` may be NULL; when set it is exported
/// as `COPAD_SOCKET` so `coctl` dials the GUI per-instance socket.
CopadHandle* copad_term_create(uint16_t cols, uint16_t rows,
                                  const char* shell, const char* cwd,
                                  const char* panel_id,
                                  const char* socket_path);
void copad_term_destroy(CopadHandle* handle);

/// `copad_term_create` for embedding apps: `set` holds `KEY=VALUE` pairs applied after copad's
/// defaults (a host's `TERM_PROGRAM` wins), `remove` keys the child must not inherit. No
/// COPAD_PANEL_ID / COPAD_SOCKET. Either list may be NULL with a count of 0.
CopadHandle* copad_term_create_env(uint16_t cols, uint16_t rows,
                                   const char* shell, const char* cwd,
                                   const char* const* set, size_t set_count,
                                   const char* const* remove, size_t remove_count);

/// The PTY child's pid, or -1 once it exited (it may be reaped and reused then).
int32_t copad_term_child_pid(CopadHandle* handle);

/// OSC 0/2 since the last call: 0 nothing new, 1 a title (*out set; free with
/// copad_string_destroy), 2 the title was reset. Last wins.
int32_t copad_term_take_title(CopadHandle* handle, CopadString** out);

/// Whether a BEL arrived since the last call (clears on read).
bool copad_term_take_bell(CopadHandle* handle);

/// Blocks until every reaper copad_term_destroy started has finished (SIGHUP to the shell's
/// session, SIGKILL to the shell after 2 s), or timeout_ms passed; true when none is left. For
/// app quit, which would otherwise cut a reaper short.
bool copad_term_wait_reaps(uint32_t timeout_ms);

void copad_term_input(CopadHandle* handle, const uint8_t* bytes, size_t len);

/// macOS only: the PTY child's cwd via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`. Returns a
/// `CopadString*` to free with `copad_string_destroy`, or NULL if the shell exited, the syscall
/// failed, or not on macOS. Works when the shell does not emit OSC 7.
CopadString* copad_term_child_cwd(CopadHandle* handle);

/// Populate one entry of the OSC color-query palette, indexed as `vte::ansi::NamedColor`: 0-15
/// ANSI, 16-255 256-color, 256 foreground, 257 background, 258 cursor. Re-call to overwrite on
/// theme hot-reload. Indices never set make OSC 4/10/11/12 queries a silent no-op.
int copad_term_set_palette_entry(CopadHandle* handle,
                                   uint16_t index,
                                   uint8_t r, uint8_t g, uint8_t b);

void copad_term_resize(CopadHandle* handle, uint16_t cols, uint16_t rows);

// True if the grid has pending damage since the last call; resets damage state. Wraps
// `copad_term_take_damage_rows`, so callers must not invoke both per frame (they share the
// internal prev-state).
bool copad_term_take_damage(CopadHandle* handle);

// Per-row damage drain: viewport rows needing repaint since the last call; resets alacritty's
// damage state. Returns -1 when a full repaint is required (scrollback offset changed,
// TermDamage::Full, or the dirty list would exceed `cap`); otherwise the exact count, with that
// many distinct row indices (unspecified order) in the first slots of `out_buf`. `out_buf` needs
// `cap` uint16_t slots when `cap > 0`; with `cap == 0` it may be NULL and -1 is returned.
int32_t copad_term_take_damage_rows(CopadHandle* handle,
                                     uint16_t* out_buf,
                                     uint16_t cap);

// True iff the PTY child exited since the last call (clears on read). The renderer broadcasts
// `panel.exited` so copad-core's ContextService cleans up per-panel cwd state.
bool copad_term_take_child_exit(CopadHandle* handle);

typedef struct {
    uint16_t start_row;
    uint16_t start_col;
    uint16_t end_row;
    uint16_t end_col;
    uint8_t present; // 0 = no active match (cleared or off-screen)
    uint8_t _reserved[3];
} CopadSearchRange;

// Find next/prev match of `pattern` in grid + scrollback. The pattern is a fixed string (the FFI
// escapes regex metacharacters). `case_sensitive` overrides alacritty's smart-case. Wraps around.
// Returns true if a match was found, after scrolling it into view.
bool copad_term_search_next(CopadHandle* handle,
                             const char* pattern,
                             bool case_sensitive,
                             bool forward);

void copad_term_search_clear(CopadHandle* handle);

// Project the snapshot's cached match into the caller's storage; `present == 0` when there is no
// match or it scrolled out of view.
void copad_snapshot_search_match(const CopadSnapshot* snap,
                                  CopadSearchRange* out);

CopadSnapshot* copad_term_snapshot(CopadHandle* handle);
void copad_snapshot_destroy(CopadSnapshot* snap);

uint16_t copad_snapshot_rows(const CopadSnapshot* snap);
uint16_t copad_snapshot_cols(const CopadSnapshot* snap);

// Sets *out_runs to a pointer borrowed from the snapshot (valid until snapshot_destroy); returns
// the run count.
size_t copad_snapshot_row_runs(const CopadSnapshot* snap, uint16_t row,
                                 const CopadRun** out_runs);

const uint8_t* copad_snapshot_row_utf8(const CopadSnapshot* snap, uint16_t row,
                                         size_t* out_len);

void copad_snapshot_cursor(const CopadSnapshot* snap, CopadCursor* out);
void copad_snapshot_selection(const CopadSnapshot* snap, CopadSelectionRange* out);

void copad_term_selection_start(CopadHandle* handle, uint16_t row, uint16_t col,
                                  uint8_t side, uint8_t kind);
void copad_term_selection_update(CopadHandle* handle, uint16_t row, uint16_t col, uint8_t side);
void copad_term_selection_clear(CopadHandle* handle);
void copad_term_selection_all(CopadHandle* handle);

// Heap-allocated UTF-8 copy of the selection, or NULL. Free with copad_string_destroy once.
CopadString* copad_term_selection_string(CopadHandle* handle);

// Last N rows of scrollback above the viewport as plain text ('\n' between rows, no trailing
// newline, NUL cells as space). N is clamped to the available rows; N==0 or no scrollback gives a
// non-NULL empty CopadString; NULL only for a NULL handle. Free with copad_string_destroy once.
CopadString* copad_term_history(CopadHandle* handle, size_t lines);

/// The live screen as text (one line per row), whatever the viewport is scrolled to; wide
/// characters' spacer cells left out. Free with copad_string_destroy.
CopadString* copad_term_screen_text(CopadHandle* handle);

/// The live screen cell by cell, whatever the viewport shows: a row's cells joined by U+001F (a
/// wide character's spacer cell empty), rows by \n; *cursor_row/*cursor_col get the live cursor.
/// Free with copad_string_destroy.
CopadString* copad_term_screen_cells(CopadHandle* handle, uint16_t* cursor_row, uint16_t* cursor_col);

const uint8_t* copad_string_bytes(const CopadString* s, size_t* out_len);
void copad_string_destroy(CopadString* s);

bool copad_term_mouse_mode_active(CopadHandle* handle);
bool copad_term_bracketed_paste_active(CopadHandle* handle);

// The kitty keyboard protocol (decisions #164). Flags the program on the visible screen
// negotiated with `CSI > flags u` (0 = none: send the legacy encoding). Off via
// copad_term_set_kitty_keyboard(false): the query goes unanswered and pushes are ignored.
uint8_t copad_term_keyboard_flags(CopadHandle* handle);
void copad_term_set_kitty_keyboard(CopadHandle* handle, bool enabled);
// Lines of scrollback to keep (`[terminal] scrollback`), clamped to 100 000. Lowering it drops
// the oldest lines for good.
void copad_term_set_scrollback(CopadHandle* handle, uint32_t lines);

typedef struct CopadKeyInput {
    uint16_t key_code;          // NSEvent.keyCode
    uint8_t mods;               // shift 1, alt 2, ctrl 4, super 8, caps lock 64
    uint8_t action;             // 0 press, 1 repeat, 2 release
    const uint8_t* unmodified;  // characters(byApplyingModifiers: []), UTF-8
    size_t unmodified_len;
    const uint8_t* shifted;     // characters(byApplyingModifiers: .shift), UTF-8
    size_t shifted_len;
    const uint8_t* text;        // the text this event types; empty for a chord
    size_t text_len;
} CopadKeyInput;

#define COPAD_KEY_TEXT    0  // send the text the normal way (also: protocol off)
#define COPAD_KEY_BYTES   1  // out[0..*out_len] holds the bytes to send
#define COPAD_KEY_NOTHING 2  // send nothing
uint8_t copad_term_encode_key(CopadHandle* handle, const CopadKeyInput* input,
                              uint8_t* out, size_t out_cap, size_t* out_len);

// Mouse encoding negotiated by the TUI; 0 when no reporting mode is on.
#define COPAD_MOUSE_ENC_NONE   0
#define COPAD_MOUSE_ENC_LEGACY 1
#define COPAD_MOUSE_ENC_SGR    2
#define COPAD_MOUSE_ENC_UTF8   3
uint8_t copad_term_mouse_encoding(CopadHandle* handle);

// Highest mouse-reporting level enabled (MOTION includes DRAG includes CLICK). Gates which
// AppKit mouse events are forwarded.
#define COPAD_MOUSE_LEVEL_NONE   0
#define COPAD_MOUSE_LEVEL_CLICK  1
#define COPAD_MOUSE_LEVEL_DRAG   2
#define COPAD_MOUSE_LEVEL_MOTION 3
uint8_t copad_term_mouse_report_level(CopadHandle* handle);

// Drain the most recent pending OSC 52 clipboard-store request, or NULL. Caller frees with
// copad_string_destroy and gates the clipboard write on the `[security] osc52` policy.
CopadString* copad_term_take_clipboard_request(CopadHandle* handle);

// Scrollback navigation; `delta` is only used for COPAD_SCROLL_DELTA (positive = older content).
#define COPAD_SCROLL_DELTA     0
#define COPAD_SCROLL_PAGE_UP   1
#define COPAD_SCROLL_PAGE_DOWN 2
#define COPAD_SCROLL_TOP       3
#define COPAD_SCROLL_BOTTOM    4
void copad_term_scroll(CopadHandle* handle, uint8_t kind, int32_t delta);

// OSC 8 hyperlink URI lookup. `hyperlink_id` is the run's 1-based index; 0 means none. URI bytes
// are borrowed from snapshot storage: copy before destroy.
uint32_t copad_snapshot_hyperlink_count(const CopadSnapshot* snap);
const uint8_t* copad_snapshot_hyperlink_uri(const CopadSnapshot* snap,
                                              uint32_t hyperlink_id,
                                              size_t* out_len);

// kitty graphics protocol (decisions #189). A snapshot lists the visible image runs in DRAW
// order (bottom-most first; z < 0 belongs under the text, z >= 0 over it) and the images they
// use. Pixels are premultiplied RGBA8, borrowed from the snapshot; `key` is unique per decoded
// image for the process, so GPU uploads can be cached by it.
typedef struct {
    float    rect[4];   // left, top, right, bottom in cells from the viewport's top-left
    float    uv[4];     // left, top, right, bottom in the image, 0..1
    int32_t  z;
    uint32_t image;     // index for copad_snapshot_graphic_image
} CopadGraphic;

typedef struct {
    uint64_t       key;
    uint32_t       width;
    uint32_t       height;
    const uint8_t* pixels;
} CopadGraphicImage;

uint32_t copad_snapshot_graphic_count(const CopadSnapshot* snap);
bool copad_snapshot_graphic(const CopadSnapshot* snap, uint32_t index, CopadGraphic* out);
bool copad_snapshot_graphic_image(const CopadSnapshot* snap, uint32_t index, CopadGraphicImage* out);

// Keyboard focus changed: sends CSI I / CSI O when the program enabled focus reports (?1004).
void copad_term_set_focused(CopadHandle* handle, bool focused);

// `[terminal] images`: off ignores every graphics command without replying.
void copad_term_set_images(CopadHandle* handle, bool enabled);
// Cell size in device pixels (images, TIOCGWINSZ, CSI 14/16 t). Call on create and on every
// font / backing-scale change.
void copad_term_set_cell_pixels(CopadHandle* handle, uint32_t width, uint32_t height);

const char* copad_term_version(void);

#endif
