// copad_ffi.h — C declarations for symbols exported by the copad-ffi staticlib.
//
// Hand-maintained to match copad-ffi/src/lib.rs. The crate has no cbindgen
// step yet because the surface is small and the spike doesn't justify the
// build-system overhead. Keep this file in lockstep with the Rust source —
// any new `extern "C"` symbol there needs a declaration here, with the same
// ownership/safety contract documented.

#ifndef COPAD_FFI_H
#define COPAD_FFI_H

#ifdef __cplusplus
extern "C" {
#endif

/// Returns a NUL-terminated static version string. DO NOT free.
const char *copad_ffi_version(void);

/// Echo a JSON string back with an `echoed_at` timestamp added. Returns a
/// heap-allocated NUL-terminated string the caller MUST free with
/// `copad_ffi_free_string`. Returns NULL on error; call `copad_ffi_last_error`
/// for the message.
char *copad_ffi_call_json(const char *input);

/// Free a string previously returned by a copad-ffi function. Pass NULL is OK.
void copad_ffi_free_string(char *s);

/// Returns the most recent error message recorded on the calling thread,
/// or NULL if none. The pointer is borrowed (do NOT free) and is invalidated
/// by the next FFI call on the same thread.
const char *copad_ffi_last_error(void);

// ---------------------------------------------------------------------------
// PR 5c — Engine FFI
//
// Wraps copad_core::trigger::TriggerEngine. Hand-maintained mirror of the
// `extern "C"` symbols in copad-ffi/src/lib.rs's "PR 5c" block. Add a
// declaration here when adding a Rust symbol; both files must stay in sync.
// ---------------------------------------------------------------------------

/// Opaque engine handle. Created by copad_engine_create, freed by
/// copad_engine_destroy. Pass through every other engine call.
typedef struct EngineHandle EngineHandle;

/// Action callback signature. Engine calls this for each trigger that
/// matches a dispatched event. user_data is whatever the host passed to
/// copad_engine_set_action_callback. action_name and params_json are
/// borrowed — host must NOT free them.
typedef void (*copad_action_callback)(
    void *user_data,
    const char *action_name,
    const char *params_json
);

EngineHandle *copad_engine_create(void);
void copad_engine_destroy(EngineHandle *handle);

/// Install / replace the action callback. NULL clears the slot.
void copad_engine_set_action_callback(
    EngineHandle *handle,
    copad_action_callback callback,
    void *user_data
);

/// Replace the trigger list. JSON shape mirrors TOML [[triggers]] entries.
/// Returns count loaded on success, -1 on parse error (use copad_ffi_last_error).
int copad_engine_set_triggers(EngineHandle *handle, const char *triggers_json);

/// Dispatch an event into the engine. Returns # of triggers fired, or -1
/// on bad input.
///
/// `source` controls the trust-boundary stamp on the synthesized Event.
/// Pass "copad.action" for registry-synthesized completion events to
/// satisfy await-promotion (see copad_core::action_registry::
/// COMPLETION_EVENT_SOURCE). Pass NULL to default to "macos.eventbus".
///
/// `context_json` is an optional `Context` snapshot for `{context.X}`
/// interpolation + condition evaluation. Wire shape matches
/// `copad_core::context::Context` serde
/// ({active_panel: string?, active_cwd: string?}). NULL → engine
/// dispatches with no context (interpolation tokens preserved literally,
/// condition references resolve to null).
/// `origin` is the trust-boundary tag (0 = Internal, 1 = External;
/// anything else defaults to Internal). Bridge consumers republishing a
/// daemon-forwarded event MUST pass through the wire `origin` so
/// `[security] accept_external` gating doesn't silently leak.
int copad_engine_dispatch_event(
    EngineHandle *handle,
    const char *event_kind,
    const char *source,
    const char *context_json,
    const char *payload_json,
    int origin
);

/// Diagnostic: number of triggers currently loaded.
int copad_engine_count_triggers(EngineHandle *handle);

// ---------------------------------------------------------------------------
// Notify FFI
//
// Wraps `copad_core::notifier::platform_notifier()` so macOS in-process
// callers (the `notify.show` registry handler) reach the same osascript
// notifier the daemon uses. Validation / truncation / subprocess spawn
// all live in core.
// ---------------------------------------------------------------------------

/// Show a desktop notification. `level` is 0=info (default), 1=warn,
/// 2=error. Returns 0 on success, 1 when no notifier is available on
/// this platform, -1 on validation / subprocess error (see
/// `copad_ffi_last_error`). `title` is required non-empty; `body` may
/// be NULL (treated as empty).
int copad_ffi_notify_show(const char *title, const char *body, int level);

// ---------------------------------------------------------------------------
// Plugin manifest FFI
//
// Validation only — directory walk / duplicate-name winner / dir
// retention stays on the caller side. JSON wire shape matches
// `copad_core::plugin::PluginManifest` (raw enum strings for
// `activation` / `restart`).
// ---------------------------------------------------------------------------

/// Read + validate `plugin.toml` at `path`. Returns a heap-allocated JSON
/// string the caller MUST free with `copad_ffi_free_string`. Returns NULL
/// on IO / parse failure; see `copad_ffi_last_error`.
char *copad_ffi_plugin_validate_toml(const char *path);

// ---------------------------------------------------------------------------
// Background FFI
//
// Read/write helpers over `copad_core::background` for wallpaper rotation.
// Callers pass the resolved paths so each platform keeps its native cache
// dir conventions. Returned image paths are owned and must be freed with
// `copad_ffi_free_string`; bool-ish results use 1/0 with -1 for errors.
// ---------------------------------------------------------------------------

/// Pick a random image path from `primary_list`, falling back to
/// `fallback_list` (may be NULL) when primary is missing or unreadable.
/// Returns a heap-allocated path string the caller MUST free with
/// `copad_ffi_free_string`. Returns NULL when neither list yields a
/// non-empty line.
char *copad_ffi_background_next_random(
    const char *primary_list,
    const char *fallback_list
);

/// 1 if rotation is active, 0 if deactive, -1 on NULL / invalid UTF-8.
/// Missing mode file = active (default).
int copad_ffi_background_is_active(const char *mode_file);

/// Flip the mode bit and persist. Returns new state (1 active, 0 deactive)
/// or -1 on NULL / invalid UTF-8.
int copad_ffi_background_toggle(const char *mode_file);

/// Remove every line exactly equal to `entry` from the wallpaper list at
/// `list` (temp-file + rename in core). Returns 1 if removed, 0 if absent
/// or list missing, -1 on NULL / invalid UTF-8 / IO error (see
/// `copad_ffi_last_error`). Backs `background.delete_current`.
int copad_ffi_background_remove_from_list(const char *list, const char *entry);

// ---------------------------------------------------------------------------
// Session FFI
//
// Argless persistence over `copad_core::session`. Wire shape matches the
// crate's serde `Session` struct (snake_case keys, type-tagged SplitSnap,
// lowercase orientation strings). Path is resolved in core
// (`paths::state_dir() / "session.json"`).
// ---------------------------------------------------------------------------

/// Load the persisted session, if any. Returns a heap-allocated JSON string
/// the caller MUST free with `copad_ffi_free_string`. Returns NULL when no
/// session exists, the file fails to parse, version is unknown, or saved
/// tab list is empty.
char *copad_ffi_session_load(void);

/// Persist a session snapshot. JSON must match the `Session` schema.
/// Returns 0 on success, -1 on NULL / non-UTF-8 / parse error (see
/// `copad_ffi_last_error`). IO errors during write are logged to stderr
/// but still return 0 — best-effort, matches Linux semantics.
int copad_ffi_session_save(const char *json);

/// Remove the persisted session file. Idempotent (NotFound is success).
/// Always returns 0; IO failures are logged to stderr.
int copad_ffi_session_clear(void);

/// Load the persisted session as the v2 model (decision #61), migrating a v1
/// file forward automatically. Heap-allocated JSON string (free with
/// copad_ffi_free_string) matching `SessionFileV2`, or NULL when absent /
/// unparseable / unknown version. Additive alongside copad_ffi_session_load.
char *copad_ffi_session_load_v2(void);

/// Persist a v2 session document. JSON must match the `SessionFileV2` schema.
/// 0 on success, -1 on NULL / non-UTF-8 / parse failure.
int copad_ffi_session_save_v2(const char *json);

// ---------------------------------------------------------------------------
// Theme FFI
//
// Read-only getters over `copad_core::theme::Theme`. Wire shape is the
// struct's serde JSON: `{name, foreground, background, palette[16],
// surface0/1/2, overlay0, text, subtext0/1, accent, red}` with hex string
// colors. Swift maps via a private `ThemeWire: Decodable`.
// ---------------------------------------------------------------------------

/// Look up a built-in theme by name. Returns a heap-allocated NUL-terminated
/// JSON string the caller MUST free with `copad_ffi_free_string`. Returns
/// NULL on unknown name; see `copad_ffi_last_error`.
char *copad_ffi_theme_get(const char *name);

/// Return a JSON array of built-in theme names. Caller MUST free with
/// `copad_ffi_free_string`.
char *copad_ffi_theme_list(void);

#ifdef __cplusplus
}
#endif

#endif // COPAD_FFI_H
