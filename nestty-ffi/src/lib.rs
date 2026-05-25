//! C-ABI bridge from `nestty_core` to platform UIs that can't link Rust
//! directly (currently `nestty-macos` via SwiftPM). Wraps `TriggerEngine`
//! so the Swift host can load triggers, dispatch events, and receive
//! action-fire callbacks without reimplementing engine semantics in Swift.
//!
//! Strings allocated on the Rust side and returned to C must be freed with
//! `nestty_ffi_free_string`; statics and thread-local error pointers must NOT.
//! Errors are reported via `nestty_ffi_last_error` (thread-local).

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nestty_core::action_registry::ActionResult;
use nestty_core::background::{self, BackgroundPaths};
use nestty_core::event_bus::Event;
use nestty_core::plugin;
use nestty_core::protocol::ResponseError;
use nestty_core::session::Session;
use nestty_core::theme::Theme;
use nestty_core::trigger::{Trigger, TriggerEngine, TriggerSink};
use serde_json::{Value, json};
use std::path::PathBuf;

thread_local! {
    /// Per-thread last-error slot. Set by entry points whose failure modes
    /// carry diagnostics (JSON parse, bad pointer, encoding errors); cleared
    /// on their success paths. Trivial entry points (handle creation /
    /// destruction, callback installation, count accessor) don't touch it.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error<S: Into<String>>(message: S) {
    let cs = CString::new(message.into()).unwrap_or_else(|_| {
        // Fallback for the (impossible) case where the message contains an
        // interior NUL. Don't lose the failure signal entirely.
        CString::new("FFI error message contained a NUL byte").unwrap()
    });
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cs));
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Pointer to a static NUL-terminated version string. Caller must NOT free.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_ffi_version() -> *const c_char {
    c"nestty-ffi 0.1.0".as_ptr()
}

/// Echo-with-`echoed_at`-timestamp round-trip. Returns a heap-allocated
/// JSON string the caller must free with `nestty_ffi_free_string`; NULL on
/// failure with the message stored in `LAST_ERROR`.
///
/// # Safety
///
/// `input` must be a valid pointer to a NUL-terminated UTF-8 string for the
/// duration of the call. The returned pointer (if non-null) must be passed
/// to `nestty_ffi_free_string` exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_call_json(input: *const c_char) -> *mut c_char {
    if input.is_null() {
        set_last_error("nestty_ffi_call_json: input pointer is NULL");
        return ptr::null_mut();
    }

    // SAFETY: caller contract requires `input` to be NUL-terminated UTF-8.
    let input_bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
    let input_str = match std::str::from_utf8(input_bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_call_json: input is not valid UTF-8: {e}"
            ));
            return ptr::null_mut();
        }
    };

    let mut parsed: Value = match serde_json::from_str(input_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_call_json: input is not valid JSON: {e}"
            ));
            return ptr::null_mut();
        }
    };

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    if let Value::Object(ref mut map) = parsed {
        map.insert("echoed_at".into(), json!(now_ms));
    } else {
        // Non-object input is allowed but loses the echo metadata; wrap it
        // so the response shape is always an object.
        parsed = json!({ "input": parsed, "echoed_at": now_ms });
    }

    let serialized = match serde_json::to_string(&parsed) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("nestty_ffi_call_json: serialization failed: {e}"));
            return ptr::null_mut();
        }
    };

    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_call_json: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };

    clear_last_error();
    cs.into_raw()
}

/// Free a string previously returned by a nestty-ffi function.
///
/// # Safety
///
/// `s` must be a pointer returned by a nestty-ffi function and not yet
/// freed, or NULL (no-op). Any other pointer is UB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: caller contract requires `s` to come from a previous nestty-ffi
    // CString::into_raw call. Reconstructing the CString hands ownership back
    // to Rust which then drops it.
    let _ = unsafe { CString::from_raw(s) };
}

/// Most recent error message on the calling thread, or NULL.
///
/// # Safety
///
/// The pointer is borrowed from a thread-local; valid only until the next
/// FFI call on the same thread. Caller must copy if retention is needed
/// (e.g. Swift `String(cString:)`). Must NOT be passed to `nestty_ffi_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_ffi_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(cs) => cs.as_ptr(),
        None => ptr::null(),
    })
}

// ============================================================================
// Engine FFI surface
// ============================================================================

/// Opaque from C — callers only ever see `*mut EngineHandle`.
pub struct EngineHandle {
    engine: Arc<TriggerEngine>,
    _sink: Arc<FfiSink>,
}

/// Forwards trigger action dispatch into a host-registered C callback.
/// Fire-and-forget: returns `{queued: true}` synchronously; real result
/// arrives async via completion-event fan-out (same shape as `LiveTriggerSink`).
struct FfiSink {
    callback: std::sync::Mutex<Option<ActionCallback>>,
    /// Stored as `usize` (not `*mut c_void`) so `FfiSink` is `Send + Sync`.
    /// Lifetime is the host's responsibility (kept alive until destroy).
    user_data: std::sync::Mutex<usize>,
}

/// Host-registered action callback. Invoked on whichever thread called
/// `nestty_engine_dispatch_event`. The `action_name` and `params_json`
/// strings are borrowed — callback must NOT free them; copy if retention needed.
pub type ActionCallback = unsafe extern "C" fn(
    user_data: *mut c_void,
    action_name: *const c_char,
    params_json: *const c_char,
);

impl TriggerSink for FfiSink {
    fn dispatch_action(&self, action: &str, params: Value) -> ActionResult {
        let cb_opt = *self.callback.lock().unwrap();
        let user = *self.user_data.lock().unwrap();
        let Some(cb) = cb_opt else {
            // No callback registered yet — log and treat as "no sink available"
            // so the engine doesn't keep retrying. Returning an Err here would
            // be cleaner but ActionResult's Err type is ResponseError which
            // requires a code/message — `{queued:false, reason:"no callback"}`
            // in Ok keeps the engine moving without polluting the error path.
            eprintln!("[nestty-ffi] dispatch_action({action}) but no Swift callback registered");
            return Ok(json!({ "queued": false, "reason": "no callback registered" }));
        };
        // Hand-rolled CString ladder. CString::new fails on NUL bytes;
        // for action names that's defensive (action keys are well-formed),
        // for params it's the caller's problem if their JSON contains NULs.
        let action_cstr = match CString::new(action) {
            Ok(c) => c,
            Err(_) => {
                return Err(ResponseError {
                    code: "ffi_error".into(),
                    message: format!("action name {action:?} contained NUL byte"),
                });
            }
        };
        let params_str = serde_json::to_string(&params).unwrap_or_else(|_| "null".to_string());
        let params_cstr = match CString::new(params_str) {
            Ok(c) => c,
            Err(_) => {
                return Err(ResponseError {
                    code: "ffi_error".into(),
                    message: "params JSON contained NUL byte".into(),
                });
            }
        };
        // SAFETY: callback is a function pointer the host registered;
        // user_data is the host-owned pointer the host promised to keep
        // alive until destroy. Both the action and params CStrings live
        // until end-of-function.
        unsafe {
            cb(
                user as *mut c_void,
                action_cstr.as_ptr(),
                params_cstr.as_ptr(),
            );
        }
        Ok(json!({ "queued": true }))
    }
}

/// Construct a fresh engine. The returned pointer must be passed to
/// `nestty_engine_destroy` exactly once, after all in-flight FFI calls
/// into the engine have returned.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_engine_create() -> *mut EngineHandle {
    let sink = Arc::new(FfiSink {
        callback: std::sync::Mutex::new(None),
        user_data: std::sync::Mutex::new(0),
    });
    let engine = Arc::new(TriggerEngine::new(sink.clone()));
    let handle = Box::new(EngineHandle {
        engine,
        _sink: sink,
    });
    Box::into_raw(handle)
}

/// # Safety
///
/// `handle` must come from `nestty_engine_create` and not have been freed.
/// Caller must ensure no other thread is mid-call into the engine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_engine_destroy(handle: *mut EngineHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees `handle` came from `Box::into_raw`
    // in `nestty_engine_create` and hasn't been freed.
    let _ = unsafe { Box::from_raw(handle) };
}

/// Install or replace the action callback. `callback = NULL` clears the slot.
///
/// # Safety
///
/// `handle` must come from `nestty_engine_create`. `user_data` must remain
/// alive until either replaced by a subsequent call OR `nestty_engine_destroy`
/// returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_engine_set_action_callback(
    handle: *mut EngineHandle,
    callback: Option<ActionCallback>,
    user_data: *mut c_void,
) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller contract.
    let h = unsafe { &*handle };
    *h._sink.callback.lock().unwrap() = callback;
    *h._sink.user_data.lock().unwrap() = user_data as usize;
}

/// Parse a JSON array of triggers and replace the engine's trigger set.
/// JSON shape matches `nestty_core::trigger::Trigger`'s Deserialize impl
/// (mirrors TOML `[[triggers]]`). Returns the loaded count, or -1 on parse
/// failure (message via `nestty_ffi_last_error`). Hot-reload semantics —
/// including the cross-lock race on await state — are documented at
/// `TriggerEngine::set_triggers`.
///
/// # Safety
///
/// `handle` must come from `nestty_engine_create`. `triggers_json` must be
/// a NUL-terminated UTF-8 string. Both must remain valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_engine_set_triggers(
    handle: *mut EngineHandle,
    triggers_json: *const c_char,
) -> i32 {
    if handle.is_null() || triggers_json.is_null() {
        set_last_error("nestty_engine_set_triggers: NULL pointer");
        return -1;
    }
    // SAFETY: caller contract.
    let h = unsafe { &*handle };
    let json_str = unsafe { CStr::from_ptr(triggers_json) }.to_string_lossy();
    let triggers: Vec<Trigger> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("nestty_engine_set_triggers: JSON parse error: {e}"));
            return -1;
        }
    };
    let count = triggers.len() as i32;
    h.engine.set_triggers(triggers);
    clear_last_error();
    count
}

/// Dispatch an event; returns the count of triggers that fired.
///
/// `source` stamps the synthesized `Event`. **Trust-boundary requirement**:
/// when synthesizing an `<action>.completed` / `<action>.failed` event for
/// await-chain promotion, `source` MUST be `COMPLETION_EVENT_SOURCE`
/// (`"nestty.action"`). Any other value causes `try_promote_or_drop_preflight`
/// to return early and silently fail to advance await state. NULL defaults
/// to `"macos.eventbus"`, which is correct for plain bus events but wrong
/// for completion-event synthesis.
///
/// `context_json` is a `nestty_core::context::Context` snapshot
/// (`{active_panel: String?, active_cwd: String?}`); NULL or empty means
/// no context (literal `{context.X}` tokens, null condition refs). Bad
/// JSON falls back to no context rather than failing the dispatch.
///
/// # Safety
///
/// `handle` must come from `nestty_engine_create`. `event_kind` must be
/// NUL-terminated UTF-8. `source`, `context_json`, `payload_json` may each
/// be NULL. All non-NULL pointers must outlive the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_engine_dispatch_event(
    handle: *mut EngineHandle,
    event_kind: *const c_char,
    source: *const c_char,
    context_json: *const c_char,
    payload_json: *const c_char,
) -> i32 {
    if handle.is_null() || event_kind.is_null() {
        set_last_error("nestty_engine_dispatch_event: NULL pointer");
        return -1;
    }
    // SAFETY: caller contract.
    let h = unsafe { &*handle };
    let kind = unsafe { CStr::from_ptr(event_kind) }
        .to_string_lossy()
        .into_owned();
    let source_str = if source.is_null() {
        "macos.eventbus".to_string()
    } else {
        unsafe { CStr::from_ptr(source) }
            .to_string_lossy()
            .into_owned()
    };
    let context: Option<nestty_core::context::Context> = if context_json.is_null() {
        None
    } else {
        let s = unsafe { CStr::from_ptr(context_json) }.to_string_lossy();
        // Empty / whitespace JSON also means "no context" — saves the
        // Swift caller a NULL/empty-dict branching.
        if s.trim().is_empty() {
            None
        } else {
            // Bad JSON falls back to None rather than failing the
            // dispatch — context is best-effort, missing fields just
            // mean `{context.X}` interpolations stay literal. Engine
            // already handles `None` gracefully.
            serde_json::from_str(&s).ok()
        }
    };
    let payload: Value = if payload_json.is_null() {
        Value::Null
    } else {
        let s = unsafe { CStr::from_ptr(payload_json) }.to_string_lossy();
        serde_json::from_str(&s).unwrap_or(Value::Null)
    };
    let event = Event::new(kind, source_str, payload);
    let fired = h.engine.dispatch(&event, context.as_ref());
    clear_last_error();
    fired as i32
}

/// Diagnostic accessor.
///
/// # Safety
///
/// `handle` must come from `nestty_engine_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_engine_count_triggers(handle: *mut EngineHandle) -> i32 {
    if handle.is_null() {
        return -1;
    }
    // SAFETY: caller contract.
    let h = unsafe { &*handle };
    h.engine.count() as i32
}

// ============================================================================
// Theme FFI surface
//
// Read-only getters over `nestty_core::theme::Theme`. Wire shape is the
// struct's serde JSON (hex string colors); ownership follows the existing
// `nestty_ffi_free_string` convention.
// ============================================================================

/// Look up a built-in theme by name and return its JSON representation.
/// Returns NULL on unknown name with the name echoed in `LAST_ERROR`.
///
/// # Safety
///
/// `name` must be a NUL-terminated UTF-8 pointer valid for the call. The
/// returned pointer (if non-null) must be passed to `nestty_ffi_free_string`
/// exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_theme_get(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        set_last_error("nestty_ffi_theme_get: name pointer is NULL");
        return ptr::null_mut();
    }
    // SAFETY: caller contract.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let name_str = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_theme_get: name is not valid UTF-8: {e}"
            ));
            return ptr::null_mut();
        }
    };
    let Some(theme) = Theme::by_name(name_str) else {
        set_last_error(format!("nestty_ffi_theme_get: unknown theme {name_str:?}"));
        return ptr::null_mut();
    };
    let serialized = match serde_json::to_string(&theme) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("nestty_ffi_theme_get: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_theme_get: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

// ============================================================================
// Notify FFI surface
//
// Lets macOS's in-process `ActionRegistry` reach the same `osascript`
// notifier the daemon uses (`nestty_core::notifier::platform_notifier`),
// so `nestctl call notify.show` works whether or not the daemon is up.
// Mirrors Linux's `register_blocking_silent("notify.show", ...)` in
// `nestty-linux/src/window.rs`.
// ============================================================================

/// Show a desktop notification via the platform notifier. `level` is
/// 0=info (default), 1=warn, 2=error; anything else treated as info.
/// Returns 0 on success, 1 when no notifier is available for this
/// platform (silent no-op), -1 on validation / subprocess error (see
/// `nestty_ffi_last_error`).
///
/// # Safety
///
/// `title` must be a non-NULL NUL-terminated UTF-8 string. `body` may
/// be NULL (treated as empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_notify_show(
    title: *const c_char,
    body: *const c_char,
    level: i32,
) -> i32 {
    if title.is_null() {
        set_last_error("nestty_ffi_notify_show: title is NULL");
        return -1;
    }
    // SAFETY: caller contract.
    let title_str = match unsafe { CStr::from_ptr(title) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_notify_show: title is not valid UTF-8: {e}"
            ));
            return -1;
        }
    };
    if title_str.is_empty() {
        set_last_error("nestty_ffi_notify_show: title must be non-empty");
        return -1;
    }
    let body_str = if body.is_null() {
        ""
    } else {
        // SAFETY: caller contract.
        match unsafe { CStr::from_ptr(body) }.to_str() {
            Ok(s) => s,
            Err(e) => {
                set_last_error(format!(
                    "nestty_ffi_notify_show: body is not valid UTF-8: {e}"
                ));
                return -1;
            }
        }
    };
    let lvl = match level {
        1 => nestty_core::notifier::Level::Warn,
        2 => nestty_core::notifier::Level::Error,
        _ => nestty_core::notifier::Level::Info,
    };
    let Some(notifier) = nestty_core::notifier::platform_notifier() else {
        clear_last_error();
        return 1;
    };
    match notifier.notify(title_str, body_str, lvl) {
        Ok(()) => {
            clear_last_error();
            0
        }
        Err(e) => {
            set_last_error(format!("nestty_ffi_notify_show: {e}"));
            -1
        }
    }
}

// ============================================================================
// Plugin manifest FFI surface
//
// Validation only — discovery (directory enumeration, duplicate-name winner
// pick, dir retention for relative `exec` / panel files) stays on the
// caller side because it varies per platform (Linux daemon vs macOS GUI
// scan their own roots). Wire shape is `nestty_core::plugin::PluginManifest`
// serialized to JSON; `Activation` / `RestartPolicy` round-trip as the raw
// `"onAction:kb.*"` / `"on-crash"` strings (see custom Serialize impls).
// ============================================================================

/// Read `plugin.toml` at `path`, parse + validate against
/// `nestty_core::plugin::PluginManifest`. Returns a heap-allocated JSON
/// string the caller must free with `nestty_ffi_free_string`. Returns
/// NULL on IO / parse failure with the diagnostic in `LAST_ERROR`.
///
/// # Safety
///
/// `path` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_plugin_validate_toml(path: *const c_char) -> *mut c_char {
    let Some(p) = (unsafe { cstr_to_pathbuf(path) }) else {
        set_last_error("nestty_ffi_plugin_validate_toml: path is NULL or invalid UTF-8");
        return ptr::null_mut();
    };
    let manifest = match plugin::validate_toml(&p) {
        Ok(m) => m,
        Err(e) => {
            set_last_error(format!("nestty_ffi_plugin_validate_toml: {e}"));
            return ptr::null_mut();
        }
    };
    let serialized = match serde_json::to_string(&manifest) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_plugin_validate_toml: serialize failed: {e}"
            ));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_plugin_validate_toml: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

// ============================================================================
// Background FFI surface
//
// Read/write helpers over `nestty_core::background`. Callers pass the
// resolved paths so each platform keeps its own conventions
// (Linux `~/.cache/...` legacy XDG vs macOS `~/Library/Caches/nestty/...`).
// ============================================================================

/// Construct a PathBuf from a C string pointer. NULL → None; invalid
/// UTF-8 → None (matches the existing FFI convention of rejecting bad
/// UTF-8 quietly rather than asserting).
unsafe fn cstr_to_pathbuf(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract — `p` is a NUL-terminated string valid for the call.
    let s = unsafe { CStr::from_ptr(p) }.to_str().ok()?;
    Some(PathBuf::from(s))
}

/// Pick a random image path from `primary_list`, falling back to
/// `fallback_list` when primary is missing/unreadable (pass NULL for no
/// fallback). Returns a heap-allocated NUL-terminated path the caller
/// must free with `nestty_ffi_free_string`. Returns NULL when neither
/// list exists or every line is blank.
///
/// # Safety
///
/// Both `primary_list` (required) and `fallback_list` (optional, may be
/// NULL) must be valid NUL-terminated UTF-8 for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_background_next_random(
    primary_list: *const c_char,
    fallback_list: *const c_char,
) -> *mut c_char {
    let Some(primary) = (unsafe { cstr_to_pathbuf(primary_list) }) else {
        set_last_error("nestty_ffi_background_next_random: primary_list is NULL or invalid UTF-8");
        return ptr::null_mut();
    };
    let fallback = unsafe { cstr_to_pathbuf(fallback_list) };
    let paths = BackgroundPaths {
        primary_list: primary,
        fallback_list: fallback,
        // Unused for this call — mode_file only matters for is_active /
        // toggle. Pass an empty path rather than threading a third arg.
        mode_file: PathBuf::new(),
    };
    let Some(picked) = background::pick_random(&paths) else {
        clear_last_error();
        return ptr::null_mut();
    };
    let cs = match CString::new(picked) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_background_next_random: path contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

/// Returns 1 if rotation mode is active, 0 if deactive, -1 on NULL /
/// invalid UTF-8 (see `nestty_ffi_last_error`). Missing mode file →
/// active (1), matches Linux default.
///
/// # Safety
///
/// `mode_file` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_background_is_active(mode_file: *const c_char) -> i32 {
    let Some(path) = (unsafe { cstr_to_pathbuf(mode_file) }) else {
        set_last_error("nestty_ffi_background_is_active: mode_file is NULL or invalid UTF-8");
        return -1;
    };
    clear_last_error();
    if background::is_active(&path) { 1 } else { 0 }
}

/// Flip the rotation mode bit and persist. Returns the new state:
/// 1 if now active, 0 if now deactive, -1 on NULL / invalid UTF-8.
/// Creates the parent directory if missing.
///
/// # Safety
///
/// `mode_file` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_background_toggle(mode_file: *const c_char) -> i32 {
    let Some(path) = (unsafe { cstr_to_pathbuf(mode_file) }) else {
        set_last_error("nestty_ffi_background_toggle: mode_file is NULL or invalid UTF-8");
        return -1;
    };
    clear_last_error();
    if background::toggle(&path) { 1 } else { 0 }
}

// ============================================================================
// Session FFI surface
//
// Argless persistence over `nestty_core::session`. Path is resolved in core
// (`paths::state_dir() / "session.json"`), so both Linux and macOS land on
// the platform's correct state dir without the wrapper having to thread a
// path string through.
// ============================================================================

/// Load the persisted session. Returns a heap-allocated JSON string the
/// caller must free with `nestty_ffi_free_string`. Returns NULL when no
/// session file exists, the file fails to parse, version is unknown, or
/// the saved tab list is empty — matching `nestty_core::session::load`.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_ffi_session_load() -> *mut c_char {
    let Some(session) = nestty_core::session::load() else {
        clear_last_error();
        return ptr::null_mut();
    };
    let serialized = match serde_json::to_string(&session) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("nestty_ffi_session_load: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_session_load: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

/// Persist a session snapshot. `json` must match the
/// `nestty_core::session::Session` schema. Returns 0 on success, -1 on
/// NULL / non-UTF-8 / JSON parse failure (diagnostics via
/// `nestty_ffi_last_error`). Underlying IO errors are logged by core to
/// stderr but still return 0 — matches Linux's best-effort save semantics.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_ffi_session_save(json: *const c_char) -> i32 {
    if json.is_null() {
        set_last_error("nestty_ffi_session_save: json pointer is NULL");
        return -1;
    }
    // SAFETY: caller contract.
    let bytes = unsafe { CStr::from_ptr(json) }.to_bytes();
    let json_str = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_session_save: input is not valid UTF-8: {e}"
            ));
            return -1;
        }
    };
    let session: Session = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("nestty_ffi_session_save: JSON parse error: {e}"));
            return -1;
        }
    };
    nestty_core::session::save(&session);
    clear_last_error();
    0
}

/// Remove the persisted session file (idempotent — `NotFound` is treated
/// as success). Always returns 0; IO failures are logged to stderr.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_ffi_session_clear() -> i32 {
    nestty_core::session::clear();
    clear_last_error();
    0
}

/// Return a JSON array of built-in theme names. Caller must free the
/// returned pointer with `nestty_ffi_free_string`.
///
/// # Safety
///
/// No input pointers; returned pointer is owned by Rust and must be freed
/// exactly once.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_ffi_theme_list() -> *mut c_char {
    let names = Theme::list();
    let serialized = match serde_json::to_string(names) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("nestty_ffi_theme_list: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "nestty_ffi_theme_list: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}
