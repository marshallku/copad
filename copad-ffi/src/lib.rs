//! C-ABI bridge from `copad_core` to platform UIs that can't link Rust
//! directly (currently `copad-macos` via SwiftPM). Wraps `TriggerEngine`
//! so the Swift host can load triggers, dispatch events, and receive
//! action-fire callbacks without reimplementing engine semantics in Swift.
//!
//! Strings allocated on the Rust side and returned to C must be freed with
//! `copad_ffi_free_string`; statics and thread-local error pointers must NOT.
//! Errors are reported via `copad_ffi_last_error` (thread-local).

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use copad_core::action_registry::ActionResult;
use copad_core::background::{self, BackgroundPaths};
use copad_core::browser;
use copad_core::event_bus::Event;
use copad_core::plugin;
use copad_core::protocol::ResponseError;
use copad_core::session::Session;
use copad_core::theme::Theme;
use copad_core::trigger::{Trigger, TriggerEngine, TriggerSink};
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
pub extern "C" fn copad_ffi_version() -> *const c_char {
    c"copad-ffi 0.1.0".as_ptr()
}

/// Echo-with-`echoed_at`-timestamp round-trip. Returns a heap-allocated
/// JSON string the caller must free with `copad_ffi_free_string`; NULL on
/// failure with the message stored in `LAST_ERROR`.
///
/// # Safety
///
/// `input` must be a valid pointer to a NUL-terminated UTF-8 string for the
/// duration of the call. The returned pointer (if non-null) must be passed
/// to `copad_ffi_free_string` exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_call_json(input: *const c_char) -> *mut c_char {
    if input.is_null() {
        set_last_error("copad_ffi_call_json: input pointer is NULL");
        return ptr::null_mut();
    }

    // SAFETY: caller contract requires `input` to be NUL-terminated UTF-8.
    let input_bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
    let input_str = match std::str::from_utf8(input_bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_call_json: input is not valid UTF-8: {e}"
            ));
            return ptr::null_mut();
        }
    };

    let mut parsed: Value = match serde_json::from_str(input_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("copad_ffi_call_json: input is not valid JSON: {e}"));
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
            set_last_error(format!("copad_ffi_call_json: serialization failed: {e}"));
            return ptr::null_mut();
        }
    };

    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_call_json: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };

    clear_last_error();
    cs.into_raw()
}

/// Free a string previously returned by a copad-ffi function.
///
/// # Safety
///
/// `s` must be a pointer returned by a copad-ffi function and not yet
/// freed, or NULL (no-op). Any other pointer is UB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: caller contract requires `s` to come from a previous copad-ffi
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
/// (e.g. Swift `String(cString:)`). Must NOT be passed to `copad_ffi_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_last_error() -> *const c_char {
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
/// `copad_engine_dispatch_event`. The `action_name` and `params_json`
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
            eprintln!("[copad-ffi] dispatch_action({action}) but no Swift callback registered");
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
/// `copad_engine_destroy` exactly once, after all in-flight FFI calls
/// into the engine have returned.
#[unsafe(no_mangle)]
pub extern "C" fn copad_engine_create() -> *mut EngineHandle {
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
/// `handle` must come from `copad_engine_create` and not have been freed.
/// Caller must ensure no other thread is mid-call into the engine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_engine_destroy(handle: *mut EngineHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller contract guarantees `handle` came from `Box::into_raw`
    // in `copad_engine_create` and hasn't been freed.
    let _ = unsafe { Box::from_raw(handle) };
}

/// Install or replace the action callback. `callback = NULL` clears the slot.
///
/// # Safety
///
/// `handle` must come from `copad_engine_create`. `user_data` must remain
/// alive until either replaced by a subsequent call OR `copad_engine_destroy`
/// returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_engine_set_action_callback(
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
/// JSON shape matches `copad_core::trigger::Trigger`'s Deserialize impl
/// (mirrors TOML `[[triggers]]`). Returns the loaded count, or -1 on parse
/// failure (message via `copad_ffi_last_error`). Hot-reload semantics —
/// including the cross-lock race on await state — are documented at
/// `TriggerEngine::set_triggers`.
///
/// # Safety
///
/// `handle` must come from `copad_engine_create`. `triggers_json` must be
/// a NUL-terminated UTF-8 string. Both must remain valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_engine_set_triggers(
    handle: *mut EngineHandle,
    triggers_json: *const c_char,
) -> i32 {
    if handle.is_null() || triggers_json.is_null() {
        set_last_error("copad_engine_set_triggers: NULL pointer");
        return -1;
    }
    // SAFETY: caller contract.
    let h = unsafe { &*handle };
    let json_str = unsafe { CStr::from_ptr(triggers_json) }.to_string_lossy();
    let triggers: Vec<Trigger> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("copad_engine_set_triggers: JSON parse error: {e}"));
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
/// (`"copad.action"`). Any other value causes `try_promote_or_drop_preflight`
/// to return early and silently fail to advance await state. NULL defaults
/// to `"macos.eventbus"`, which is correct for plain bus events but wrong
/// for completion-event synthesis.
///
/// `origin` carries the trust-boundary tag the engine's `[security]
/// accept_external` gate consumes (0 = Internal, 1 = External; any other
/// value defaults to Internal as the safe choice). When the macOS GUI
/// republishes a daemon-forwarded event, this MUST match the wire
/// `origin` parsed from the bridge — otherwise an `External` event
/// would launder into a trusted local trigger and bypass the gate.
///
/// `context_json` is a `copad_core::context::Context` snapshot
/// (`{active_panel: String?, active_cwd: String?}`); NULL or empty means
/// no context (literal `{context.X}` tokens, null condition refs). Bad
/// JSON falls back to no context rather than failing the dispatch.
///
/// # Safety
///
/// `handle` must come from `copad_engine_create`. `event_kind` must be
/// NUL-terminated UTF-8. `source`, `context_json`, `payload_json` may each
/// be NULL. All non-NULL pointers must outlive the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_engine_dispatch_event(
    handle: *mut EngineHandle,
    event_kind: *const c_char,
    source: *const c_char,
    context_json: *const c_char,
    payload_json: *const c_char,
    origin: i32,
) -> i32 {
    if handle.is_null() || event_kind.is_null() {
        set_last_error("copad_engine_dispatch_event: NULL pointer");
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
    let context: Option<copad_core::context::Context> = if context_json.is_null() {
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
    let origin_tag = match origin {
        1 => copad_core::event_bus::Origin::External,
        // 0 + any unknown value → safe default. Refusing the dispatch on
        // a malformed origin would silently drop events at a protocol
        // bump; routing to Internal keeps local triggers firing while
        // the External path stays opt-in.
        _ => copad_core::event_bus::Origin::Internal,
    };
    let event = Event::new(kind, source_str, payload).with_origin(origin_tag);
    let fired = h.engine.dispatch(&event, context.as_ref());
    clear_last_error();
    fired as i32
}

/// Diagnostic accessor.
///
/// # Safety
///
/// `handle` must come from `copad_engine_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_engine_count_triggers(handle: *mut EngineHandle) -> i32 {
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
// Read-only getters over `copad_core::theme::Theme`. Wire shape is the
// struct's serde JSON (hex string colors); ownership follows the existing
// `copad_ffi_free_string` convention.
// ============================================================================

/// Look up a built-in theme by name and return its JSON representation.
/// Returns NULL on unknown name with the name echoed in `LAST_ERROR`.
///
/// # Safety
///
/// `name` must be a NUL-terminated UTF-8 pointer valid for the call. The
/// returned pointer (if non-null) must be passed to `copad_ffi_free_string`
/// exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_theme_get(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        set_last_error("copad_ffi_theme_get: name pointer is NULL");
        return ptr::null_mut();
    }
    // SAFETY: caller contract.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let name_str = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("copad_ffi_theme_get: name is not valid UTF-8: {e}"));
            return ptr::null_mut();
        }
    };
    let Some(theme) = Theme::by_name(name_str) else {
        set_last_error(format!("copad_ffi_theme_get: unknown theme {name_str:?}"));
        return ptr::null_mut();
    };
    let serialized = match serde_json::to_string(&theme) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("copad_ffi_theme_get: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_theme_get: serialized JSON contained NUL byte: {e}"
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
// notifier the daemon uses (`copad_core::notifier::platform_notifier`),
// so `coctl call notify.show` works whether or not the daemon is up.
// Mirrors Linux's `register_blocking_silent("notify.show", ...)` in
// `copad-linux/src/window.rs`.
// ============================================================================

/// Show a desktop notification via the platform notifier. `level` is
/// 0=info (default), 1=warn, 2=error; anything else treated as info.
/// Returns 0 on success, 1 when no notifier is available for this
/// platform (silent no-op), -1 on validation / subprocess error (see
/// `copad_ffi_last_error`).
///
/// # Safety
///
/// `title` must be a non-NULL NUL-terminated UTF-8 string. `body` may
/// be NULL (treated as empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_notify_show(
    title: *const c_char,
    body: *const c_char,
    level: i32,
) -> i32 {
    if title.is_null() {
        set_last_error("copad_ffi_notify_show: title is NULL");
        return -1;
    }
    // SAFETY: caller contract.
    let title_str = match unsafe { CStr::from_ptr(title) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_notify_show: title is not valid UTF-8: {e}"
            ));
            return -1;
        }
    };
    if title_str.is_empty() {
        set_last_error("copad_ffi_notify_show: title must be non-empty");
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
                    "copad_ffi_notify_show: body is not valid UTF-8: {e}"
                ));
                return -1;
            }
        }
    };
    let lvl = match level {
        1 => copad_core::notifier::Level::Warn,
        2 => copad_core::notifier::Level::Error,
        _ => copad_core::notifier::Level::Info,
    };
    let Some(notifier) = copad_core::notifier::platform_notifier() else {
        clear_last_error();
        return 1;
    };
    match notifier.notify(title_str, body_str, lvl) {
        Ok(()) => {
            clear_last_error();
            0
        }
        Err(e) => {
            set_last_error(format!("copad_ffi_notify_show: {e}"));
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
// scan their own roots). Wire shape is `copad_core::plugin::PluginManifest`
// serialized to JSON; `Activation` / `RestartPolicy` round-trip as the raw
// `"onAction:kb.*"` / `"on-crash"` strings (see custom Serialize impls).
// ============================================================================

/// Read `plugin.toml` at `path`, parse + validate against
/// `copad_core::plugin::PluginManifest`. Returns a heap-allocated JSON
/// string the caller must free with `copad_ffi_free_string`. Returns
/// NULL on IO / parse failure with the diagnostic in `LAST_ERROR`.
///
/// # Safety
///
/// `path` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_plugin_validate_toml(path: *const c_char) -> *mut c_char {
    let Some(p) = (unsafe { cstr_to_pathbuf(path) }) else {
        set_last_error("copad_ffi_plugin_validate_toml: path is NULL or invalid UTF-8");
        return ptr::null_mut();
    };
    let manifest = match plugin::validate_toml(&p) {
        Ok(m) => m,
        Err(e) => {
            set_last_error(format!("copad_ffi_plugin_validate_toml: {e}"));
            return ptr::null_mut();
        }
    };
    let serialized = match serde_json::to_string(&manifest) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_plugin_validate_toml: serialize failed: {e}"
            ));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_plugin_validate_toml: serialized JSON contained NUL byte: {e}"
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
// Read/write helpers over `copad_core::background`. Callers pass the
// resolved paths so each platform keeps its own conventions
// (Linux `~/.cache/...` legacy XDG vs macOS `~/Library/Caches/copad/...`).
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
/// must free with `copad_ffi_free_string`. Returns NULL when neither
/// list exists or every line is blank.
///
/// # Safety
///
/// Both `primary_list` (required) and `fallback_list` (optional, may be
/// NULL) must be valid NUL-terminated UTF-8 for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_background_next_random(
    primary_list: *const c_char,
    fallback_list: *const c_char,
) -> *mut c_char {
    let Some(primary) = (unsafe { cstr_to_pathbuf(primary_list) }) else {
        set_last_error("copad_ffi_background_next_random: primary_list is NULL or invalid UTF-8");
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
                "copad_ffi_background_next_random: path contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

/// Returns 1 if rotation mode is active, 0 if deactive, -1 on NULL /
/// invalid UTF-8 (see `copad_ffi_last_error`). Missing mode file →
/// active (1), matches Linux default.
///
/// # Safety
///
/// `mode_file` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_background_is_active(mode_file: *const c_char) -> i32 {
    let Some(path) = (unsafe { cstr_to_pathbuf(mode_file) }) else {
        set_last_error("copad_ffi_background_is_active: mode_file is NULL or invalid UTF-8");
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
pub unsafe extern "C" fn copad_ffi_background_toggle(mode_file: *const c_char) -> i32 {
    let Some(path) = (unsafe { cstr_to_pathbuf(mode_file) }) else {
        set_last_error("copad_ffi_background_toggle: mode_file is NULL or invalid UTF-8");
        return -1;
    };
    clear_last_error();
    if background::toggle(&path) { 1 } else { 0 }
}

/// Remove every line exactly equal to `entry` from the wallpaper list at
/// `list`. Returns 1 if something was removed, 0 if the entry wasn't
/// present (or the list is missing), -1 on NULL / invalid UTF-8 / IO
/// error (see `copad_ffi_last_error`). Backs `background.delete_current`
/// on macOS — the rewrite is temp-file + rename in core so a crash can't
/// truncate the catalog.
///
/// # Safety
///
/// Both `list` and `entry` must be NUL-terminated UTF-8 pointers valid
/// for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_background_remove_from_list(
    list: *const c_char,
    entry: *const c_char,
) -> i32 {
    let Some(list_path) = (unsafe { cstr_to_pathbuf(list) }) else {
        set_last_error("copad_ffi_background_remove_from_list: list is NULL or invalid UTF-8");
        return -1;
    };
    let Some(entry_path) = (unsafe { cstr_to_pathbuf(entry) }) else {
        set_last_error("copad_ffi_background_remove_from_list: entry is NULL or invalid UTF-8");
        return -1;
    };
    match background::remove_from_list(&list_path, &entry_path.to_string_lossy()) {
        Ok(removed) => {
            clear_last_error();
            if removed { 1 } else { 0 }
        }
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_background_remove_from_list: {}: {e}",
                list_path.display()
            ));
            -1
        }
    }
}

// ============================================================================
// Session FFI surface
//
// Argless persistence over `copad_core::session`. Path is resolved in core
// (`paths::state_dir() / "session.json"`), so both Linux and macOS land on
// the platform's correct state dir without the wrapper having to thread a
// path string through.
// ============================================================================

/// Load the persisted session. Returns a heap-allocated JSON string the
/// caller must free with `copad_ffi_free_string`. Returns NULL when no
/// session file exists, the file fails to parse, version is unknown, or
/// the saved tab list is empty — matching `copad_core::session::load`.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_session_load() -> *mut c_char {
    let Some(session) = copad_core::session::load() else {
        clear_last_error();
        return ptr::null_mut();
    };
    let serialized = match serde_json::to_string(&session) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("copad_ffi_session_load: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_session_load: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

/// Persist a session snapshot. `json` must match the
/// `copad_core::session::Session` schema. Returns 0 on success, -1 on
/// NULL / non-UTF-8 / JSON parse failure (diagnostics via
/// `copad_ffi_last_error`). Underlying IO errors are logged by core to
/// stderr but still return 0 — matches Linux's best-effort save semantics.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_session_save(json: *const c_char) -> i32 {
    if json.is_null() {
        set_last_error("copad_ffi_session_save: json pointer is NULL");
        return -1;
    }
    // SAFETY: caller contract.
    let bytes = unsafe { CStr::from_ptr(json) }.to_bytes();
    let json_str = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_session_save: input is not valid UTF-8: {e}"
            ));
            return -1;
        }
    };
    let session: Session = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            set_last_error(format!("copad_ffi_session_save: JSON parse error: {e}"));
            return -1;
        }
    };
    copad_core::session::save(&session);
    clear_last_error();
    0
}

/// Remove the persisted session file (idempotent — `NotFound` is treated
/// as success). Always returns 0; IO failures are logged to stderr.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_session_clear() -> i32 {
    copad_core::session::clear();
    clear_last_error();
    0
}

/// Return a JSON array of built-in theme names. Caller must free the
/// returned pointer with `copad_ffi_free_string`.
///
/// # Safety
///
/// No input pointers; returned pointer is owned by Rust and must be freed
/// exactly once.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_theme_list() -> *mut c_char {
    let names = Theme::list();
    let serialized = match serde_json::to_string(names) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("copad_ffi_theme_list: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    let cs = match CString::new(serialized) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!(
                "copad_ffi_theme_list: serialized JSON contained NUL byte: {e}"
            ));
            return ptr::null_mut();
        }
    };
    clear_last_error();
    cs.into_raw()
}

// ============================================================================
// Browser Workbench (docs/browser-workbench-plan.md, decision #100)
//
// Three entry points, and deliberately only three. The browser DATA types are
// fine for Swift to mirror — `Session.swift` already mirrors the whole session
// model — but the browser RULES are not, because a second implementation of a
// security rule is a second implementation that can be wrong on its own. Every
// bug B1a fixed in `canonical_origin` (backslash authority smuggling,
// percent-encoded token keys) still lived in the Swift copy this replaces.
//
// Each takes and returns one JSON object, in the established
// `*mut c_char` / `copad_ffi_free_string` / `copad_ffi_last_error` shape.
// ============================================================================

/// Read a NUL-terminated UTF-8 JSON argument into a `serde_json::Value`.
/// Shared by the browser entry points so each one's failure diagnostics
/// name itself consistently.
///
/// # Safety
///
/// `json` must be NULL or a NUL-terminated UTF-8 pointer valid for the call.
unsafe fn parse_json_arg(fn_name: &str, json: *const c_char) -> Option<Value> {
    if json.is_null() {
        set_last_error(format!("{fn_name}: json pointer is NULL"));
        return None;
    }
    // SAFETY: caller contract.
    let bytes = unsafe { CStr::from_ptr(json) }.to_bytes();
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("{fn_name}: input is not valid UTF-8: {e}"));
            return None;
        }
    };
    match serde_json::from_str(s) {
        Ok(v) => Some(v),
        Err(e) => {
            set_last_error(format!("{fn_name}: JSON parse error: {e}"));
            None
        }
    }
}

/// Serialize a response value into an owned C string.
fn json_response(fn_name: &str, value: &Value) -> *mut c_char {
    let serialized = match serde_json::to_string(value) {
        Ok(s) => s,
        Err(e) => {
            set_last_error(format!("{fn_name}: serialize failed: {e}"));
            return ptr::null_mut();
        }
    };
    match CString::new(serialized) {
        Ok(c) => {
            clear_last_error();
            c.into_raw()
        }
        Err(e) => {
            set_last_error(format!("{fn_name}: response contained NUL byte: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ url, policy }` → `{ url, persist_title }`.
///
/// Applies `[browser] restore` to a live URL. An unknown `policy` string falls
/// back to `origin` — the safe direction, matching `BrowserConfig::restore_policy`
/// — because a typo must never silently widen what reaches disk.
///
/// `persist_title` is returned because a page TITLE carries the same exposure as
/// a URL ("Reset password for …"), and the caller must not re-derive it by
/// comparing the raw policy string itself: `"ORIGIN"`, `" origin "` and a typo
/// all resolve to origin-only HERE, and a second interpretation on the Swift
/// side got that wrong. One parse, one answer.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_canonicalize(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_canonicalize";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let url = input.get("url").and_then(Value::as_str).unwrap_or("");
    let policy = input
        .get("policy")
        .and_then(Value::as_str)
        .and_then(browser::RestorePolicy::parse)
        .unwrap_or_default();
    json_response(
        NAME,
        &json!({
            "url": browser::canonicalize_for_restore(url, policy),
            "persist_title": policy.is_sensitive(),
            "keeps_history": policy.keeps_history(),
        }),
    )
}

/// `{ pane }` → `{ pane, repairs }`.
///
/// Runs an untrusted `BrowserPaneSnap` through `browser::tabs::normalize`, which
/// is what keeps a hand-edited or imported session document from becoming
/// filesystem authority. `repairs` reports what had to be fixed so a caller can
/// decide whether to tell the user.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_normalize(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_normalize";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let Some(pane_value) = input.get("pane") else {
        set_last_error(format!("{NAME}: missing 'pane'"));
        return ptr::null_mut();
    };
    let mut pane: browser::BrowserPaneSnap = match serde_json::from_value(pane_value.clone()) {
        Ok(p) => p,
        Err(e) => {
            set_last_error(format!("{NAME}: 'pane' does not match the schema: {e}"));
            return ptr::null_mut();
        }
    };
    let repairs = browser::tabs::normalize(&mut pane);
    json_response(
        NAME,
        &json!({
            "pane": pane,
            "repairs": {
                "dropped_invalid_id": repairs.dropped_invalid_id,
                "dropped_duplicate_id": repairs.dropped_duplicate_id,
                "dropped_over_cap": repairs.dropped_over_cap,
                "clamped_active": repairs.clamped_active,
                "clean": repairs.is_clean(),
            }
        }),
    )
}

/// `{ method, mode, profile_protected }` → `{ allowed, opaque_write, code?, message? }`.
///
/// One call answers both questions a dispatcher has: may this run, and must its
/// result be replaced with a page-independent one. Returning them together is
/// what lets the caller re-ask cheaply at delivery time — and it must, because a
/// read that was already in flight when a tab entered `protected` has to be
/// suppressed rather than answered.
///
/// An unrecognised `mode` is treated as `protected`, i.e. the restrictive
/// reading: a caller that cannot say what state a tab is in must not be told
/// the tab is open for automation.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_authorize(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_authorize";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let method = input.get("method").and_then(Value::as_str).unwrap_or("");
    let mode = match input.get("mode").and_then(Value::as_str) {
        Some("automation") => browser::TabMode::Automation,
        // Anything else — including a missing or misspelled value — reads as
        // protected. Fail closed.
        _ => browser::TabMode::Protected,
    };
    let profile_protected = input
        .get("profile_protected")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let opaque_write = browser::authorize::redacts_write_result(method, mode, profile_protected);
    match browser::authorize(method, mode, profile_protected) {
        Ok(()) => json_response(
            NAME,
            &json!({ "allowed": true, "opaque_write": opaque_write }),
        ),
        Err(e) => json_response(
            NAME,
            &json!({
                "allowed": false,
                "opaque_write": opaque_write,
                "code": e.code,
                "message": e.message,
            }),
        ),
    }
}

/// `{ tab_id, generation, data_hex }` → `{ ok }`.
///
/// Persist a tab's opaque back/forward + scroll blob as a NEW generation. The
/// caller writes `session.json` referencing that generation only after this
/// returns, so a crash between the two cannot pair old metadata with new
/// history.
///
/// Hex rather than base64 because `copad-core` carries no base64 dependency and
/// the workspace's only implementation is a private encoder with no decoder.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_history_write(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_history_write";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let tab_id = input.get("tab_id").and_then(Value::as_str).unwrap_or("");
    let generation = input
        .get("generation")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let hex = input.get("data_hex").and_then(Value::as_str).unwrap_or("");
    let data = match browser::history::hex_decode(hex) {
        Ok(d) => d,
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            return ptr::null_mut();
        }
    };
    match browser::history::write(tab_id, generation, &data) {
        Ok(()) => json_response(NAME, &json!({ "ok": true })),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ tab_id, generation }` → `{ data_hex }`.
///
/// NULL when the blob is absent, unreadable, oversize, or a symlink was planted
/// at its name — all of which the caller treats identically: fall back to a
/// plain URL load rather than restoring a history it cannot trust.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_history_read(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_history_read";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let tab_id = input.get("tab_id").and_then(Value::as_str).unwrap_or("");
    let generation = input
        .get("generation")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    match browser::history::read(tab_id, generation) {
        Ok(data) => json_response(
            NAME,
            &json!({ "data_hex": browser::history::hex_encode(&data) }),
        ),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ live: [[tab_id, generation], …] }` → `{ removed }`.
///
/// Reclaim superseded generations after a session has committed. Only files
/// matching the exact blob grammar are candidates — this is not a directory
/// wipe, so anything else that ends up there survives.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_history_gc(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_history_gc";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let mut live: Vec<(String, u64)> = Vec::new();
    if let Some(arr) = input.get("live").and_then(Value::as_array) {
        for pair in arr {
            let Some(pair) = pair.as_array() else {
                continue;
            };
            let (Some(id), Some(generation)) = (
                pair.first().and_then(Value::as_str),
                pair.get(1).and_then(Value::as_u64),
            ) else {
                continue;
            };
            live.push((id.to_string(), generation));
        }
    }
    json_response(NAME, &json!({ "removed": browser::history::gc(&live) }))
}

fn log_caps_from(input: &Value) -> browser::LogCaps {
    let mut caps = browser::LogCaps::default();
    if let Some(v) = input.get("capture_bodies").and_then(Value::as_bool) {
        caps.capture_bodies = v;
    }
    caps
}

/// `{ panel_id, kind, record, capture_bodies? }` → `{ written }`.
///
/// `written: false` means the record was DROPPED for exceeding the per-record
/// cap even after shedding. It is reported rather than swallowed because a
/// caller that treats a drop as success is silently under-reporting what the
/// page did.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_netlog_append(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_netlog_append";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let panel_id = input.get("panel_id").and_then(Value::as_str).unwrap_or("");
    let caps = log_caps_from(&input);
    let Some(record) = input.get("record") else {
        set_last_error(format!("{NAME}: missing 'record'"));
        return ptr::null_mut();
    };
    let kind = input.get("kind").and_then(Value::as_str).unwrap_or("");
    let written = match kind {
        "net" => match serde_json::from_value::<browser::NetRecord>(record.clone()) {
            Ok(r) => browser::netlog::append_net(panel_id, r, &caps),
            Err(e) => Err(format!("record does not match the net schema: {e}")),
        },
        "console" => match serde_json::from_value::<browser::ConsoleRecord>(record.clone()) {
            Ok(r) => browser::netlog::append_console(panel_id, r, &caps),
            Err(e) => Err(format!("record does not match the console schema: {e}")),
        },
        other => Err(format!("unknown record kind: {other:?}")),
    };
    match written {
        Ok(ok) => json_response(NAME, &json!({ "written": ok })),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ panel_id, kind?, since?, tab_id?, level?, contains?, limit? }`
/// → `{ records, coverage }`.
///
/// `coverage` is returned on EVERY read, not documented once and forgotten:
/// patching `fetch`/`XHR` is not a packet log, and an agent reading an empty
/// list must not conclude "no request was made".
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_netlog_read(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_netlog_read";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let panel_id = input.get("panel_id").and_then(Value::as_str).unwrap_or("");
    let query = browser::netlog::ReadQuery {
        kind: match input.get("kind").and_then(Value::as_str) {
            Some("net") => Some(browser::netlog::Kind::Net),
            Some("console") => Some(browser::netlog::Kind::Console),
            _ => None,
        },
        since: input.get("since").and_then(Value::as_u64),
        tab_id: input
            .get("tab_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        level: input
            .get("level")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        contains: input
            .get("contains")
            .and_then(Value::as_str)
            .map(str::to_string),
        limit: input.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize,
    };
    match browser::netlog::read(panel_id, &query) {
        Ok(records) => json_response(
            NAME,
            &json!({ "records": records, "coverage": browser::NETLOG_COVERAGE }),
        ),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ panel_id }` → `{ removed }`.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_netlog_clear(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_netlog_clear";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let panel_id = input.get("panel_id").and_then(Value::as_str).unwrap_or("");
    match browser::netlog::clear(panel_id) {
        Ok(removed) => json_response(NAME, &json!({ "removed": removed })),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ origin? }` → `{ credentials }`. Metadata only — see `CredentialRef`.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_credentials_list(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_credentials_list";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let origin = input.get("origin").and_then(Value::as_str);
    json_response(
        NAME,
        &json!({ "credentials": browser::secrets::list(origin) }),
    )
}

/// `{ credential }` → `{ ok }`. Add or replace one index entry.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_credentials_upsert(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_credentials_upsert";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let Some(value) = input.get("credential") else {
        set_last_error(format!("{NAME}: missing 'credential'"));
        return ptr::null_mut();
    };
    let entry: browser::CredentialRef = match serde_json::from_value(value.clone()) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!("{NAME}: credential does not match the schema: {e}"));
            return ptr::null_mut();
        }
    };
    if !browser::secrets::is_valid_credential_id(&entry.id) {
        set_last_error(format!("{NAME}: invalid credential id: {:?}", entry.id));
        return ptr::null_mut();
    }
    match browser::secrets::upsert(entry) {
        Ok(_) => json_response(NAME, &json!({ "ok": true })),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ credential_id }` → `{ removed }`.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_credentials_remove(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_credentials_remove";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };
    let id = input
        .get("credential_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    match browser::secrets::remove(id) {
        Ok(removed) => json_response(NAME, &json!({ "removed": removed })),
        Err(e) => {
            set_last_error(format!("{NAME}: {e}"));
            ptr::null_mut()
        }
    }
}

/// `{ request, credential, live, target }` → `{ ok }` or NULL with the reason.
///
/// The credential-fill preconditions, evaluated by `copad_core` rather than
/// re-implemented per platform. That matters more here than anywhere else in
/// this surface: these are the checks that decide whether a password is written
/// into a page, and a second implementation is a second thing that can be
/// subtly wrong on its own.
///
/// `target` is the element as observed in the SAME synchronous step that will
/// perform the injection — never a value captured earlier, because a page can
/// swap the input or flip its `type` without navigating.
///
/// # Safety
///
/// `json` must be a NUL-terminated UTF-8 pointer valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_ffi_browser_validate_fill(json: *const c_char) -> *mut c_char {
    const NAME: &str = "copad_ffi_browser_validate_fill";
    // SAFETY: caller contract, forwarded.
    let Some(input) = (unsafe { parse_json_arg(NAME, json) }) else {
        return ptr::null_mut();
    };

    let get = |key: &str| input.get(key).cloned().unwrap_or(Value::Null);
    let slot = |v: &Value| match v.as_str() {
        Some("username") => browser::CredentialSlot::Username,
        _ => browser::CredentialSlot::Password,
    };

    let cred: browser::CredentialRef = match serde_json::from_value(get("credential")) {
        Ok(c) => c,
        Err(e) => {
            set_last_error(format!("{NAME}: credential does not match the schema: {e}"));
            return ptr::null_mut();
        }
    };
    let req_v = get("request");
    let live_v = get("live");
    let target_v = get("target");

    let req = browser::FillRequest {
        credential_id: req_v
            .get("credential_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        profile: req_v
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tab_id: req_v
            .get("tab_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        origin: req_v
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        document_generation: req_v
            .get("document_generation")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        slot: slot(&req_v.get("slot").cloned().unwrap_or(Value::Null)),
    };
    let live = browser::secrets::TabState {
        tab_id: live_v
            .get("tab_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        profile: live_v
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // An unrecognised mode reads as Automation here, which REFUSES the
        // fill — the restrictive direction.
        mode: match live_v.get("mode").and_then(Value::as_str) {
            Some("protected") => browser::TabMode::Protected,
            _ => browser::TabMode::Automation,
        },
        origin: live_v
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        document_generation: live_v
            .get("document_generation")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    };
    let target = browser::secrets::LiveTarget {
        selector: target_v
            .get("selector")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        is_password_input: target_v
            .get("is_password_input")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };

    match browser::secrets::validate_fill(&req, &cred, &live, &target) {
        Ok(()) => json_response(NAME, &json!({ "ok": true })),
        Err(e) => json_response(
            NAME,
            &json!({ "ok": false, "code": e.code, "message": e.message }),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// comux host (decisions #169): the macOS GUI draws comux's sidebar natively. No callback
// crosses into Swift — the GUI polls `copad_muxhost_generation` and re-reads the state when it
// moves, so a window closing can never race a call into freed Swift memory.
// ---------------------------------------------------------------------------------------------

/// Opaque handle to a running [`copad_core::muxhost::MuxHost`].
pub struct MuxHostHandle(copad_core::muxhost::MuxHost);

/// Borrow a C string as `&str` (`None` for NULL or invalid UTF-8).
///
/// # Safety
/// `p` must be NULL or a valid NUL-terminated string for the duration of the borrow.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn into_c(s: String) -> *mut c_char {
    CString::new(s).map_or(ptr::null_mut(), CString::into_raw)
}

/// Start a host on the LOCAL comux socket (`dest` NULL) or on `ssh -T <dest> comux proxy
/// --stdio` (P4). Free with `copad_muxhost_destroy`.
///
/// # Safety
/// `dest` must be NULL or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_start(dest: *const c_char) -> *mut MuxHostHandle {
    use copad_mux_proto::link::Endpoint;
    // SAFETY: caller contract.
    let ep = match unsafe { cstr(dest) } {
        Some(d) if !d.is_empty() => Endpoint::ssh(d),
        _ => Endpoint::local(),
    };
    Box::into_raw(Box::new(MuxHostHandle(
        copad_core::muxhost::MuxHost::start(ep),
    )))
}

/// # Safety
/// `h` must come from `copad_muxhost_start` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_destroy(h: *mut MuxHostHandle) {
    if !h.is_null() {
        // SAFETY: caller contract.
        drop(unsafe { Box::from_raw(h) });
    }
}

/// Moves whenever anything the GUI could show changed.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_generation(h: *const MuxHostHandle) -> u64 {
    // SAFETY: caller contract.
    unsafe { h.as_ref() }.map_or(0, |h| h.0.generation())
}

/// # Safety
/// `h` live; `panel`, `tag` valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_bind(
    h: *const MuxHostHandle,
    panel: *const c_char,
    tag: *const c_char,
) {
    // SAFETY: caller contract.
    if let (Some(h), Some(p), Some(t)) = (unsafe { h.as_ref() }, unsafe { cstr(panel) }, unsafe {
        cstr(tag)
    }) {
        h.0.bind(p, t);
    }
}

/// # Safety
/// `h` live; `panel` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_unbind(h: *const MuxHostHandle, panel: *const c_char) {
    // SAFETY: caller contract.
    if let (Some(h), Some(p)) = (unsafe { h.as_ref() }, unsafe { cstr(panel) }) {
        h.0.unbind(p);
    }
}

/// The panels on screen, as a JSON array of panel ids.
///
/// # Safety
/// `h` live; `panels_json` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_serve(h: *const MuxHostHandle, panels_json: *const c_char) {
    // SAFETY: caller contract.
    if let (Some(h), Some(j)) = (unsafe { h.as_ref() }, unsafe { cstr(panels_json) })
        && let Ok(panels) = serde_json::from_str::<Vec<String>>(j)
    {
        h.0.serve(&panels);
    }
}

/// The state as JSON (see `MuxHost::state_json`). Free with `copad_ffi_free_string`.
///
/// # Safety
/// `h` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_state_json(h: *const MuxHostHandle) -> *mut c_char {
    // SAFETY: caller contract.
    unsafe { h.as_ref() }.map_or(ptr::null_mut(), |h| into_c(h.0.state_json().to_string()))
}

/// Run a comux control request (`{"cmd":…}` JSON).
///
/// # Safety
/// `h` live; `req_json` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_command(h: *const MuxHostHandle, req_json: *const c_char) {
    // SAFETY: caller contract.
    if let (Some(h), Some(j)) = (unsafe { h.as_ref() }, unsafe { cstr(req_json) })
        && let Ok(v) = serde_json::from_str(j)
    {
        h.0.command(v);
    }
}

/// Ask comux to open one of its overlays (`what`: `new-session`, `resume`, `rename-session`,
/// …) for `panel`'s client; `target` NULL = the active session/tab (decisions #172).
///
/// # Safety
/// `h` live; `panel`, `what` valid strings; `target` NULL or a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_open(
    h: *const MuxHostHandle,
    panel: *const c_char,
    what: *const c_char,
    target: *const c_char,
) {
    // SAFETY: caller contract.
    if let (Some(h), Some(p), Some(w)) = (unsafe { h.as_ref() }, unsafe { cstr(panel) }, unsafe {
        cstr(what)
    }) {
        let target = unsafe { cstr(target) }.map(str::to_string);
        h.0.open(p, w, target);
    }
}

/// Start a host on a REMOTE comux: `ssh -T <dest> <comux> proxy --stdio`. NULL when the
/// destination or command is refused (see `copad_core::muxhost::valid_dest`).
///
/// # Safety
/// Both arguments valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_start_remote(
    dest: *const c_char,
    comux: *const c_char,
) -> *mut MuxHostHandle {
    // SAFETY: caller contract.
    let (Some(d), Some(c)) = (unsafe { cstr(dest) }, unsafe { cstr(comux) }) else {
        return ptr::null_mut();
    };
    match copad_core::muxhost::remote_endpoint(d, c) {
        Some(ep) => Box::into_raw(Box::new(MuxHostHandle(
            copad_core::muxhost::MuxHost::start(ep),
        ))),
        None => ptr::null_mut(),
    }
}

/// A fresh binding tag for a remote tab. Free with `copad_ffi_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn copad_muxhost_new_remote_tag() -> *mut c_char {
    into_c(copad_core::muxhost::new_remote_tag())
}

/// The command a copad tab runs to attach the remote comux with `tag`. NULL when refused.
/// Free with `copad_ffi_free_string`.
///
/// # Safety
/// All arguments valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_remote_attach_command(
    dest: *const c_char,
    comux: *const c_char,
    tag: *const c_char,
) -> *mut c_char {
    // SAFETY: caller contract.
    match (unsafe { cstr(dest) }, unsafe { cstr(comux) }, unsafe {
        cstr(tag)
    }) {
        (Some(d), Some(c), Some(t)) => {
            copad_core::muxhost::remote_attach_command(d, c, t).map_or(ptr::null_mut(), into_c)
        }
        _ => ptr::null_mut(),
    }
}

/// Answer a delegated overlay. `choice`/`text` NULL when absent; both NULL = cancelled.
///
/// # Safety
/// `h` live; `choice`, `text` NULL or valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_answer(
    h: *const MuxHostHandle,
    overlay: u64,
    choice: *const c_char,
    text: *const c_char,
) {
    // SAFETY: caller contract.
    if let Some(h) = unsafe { h.as_ref() } {
        let choice = unsafe { cstr(choice) }.map(str::to_string);
        let text = unsafe { cstr(text) }.map(str::to_string);
        h.0.answer(overlay, choice, text);
    }
}

/// The binding tag a comux client in this panel reports. Free with `copad_ffi_free_string`.
///
/// # Safety
/// Both arguments valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_copad_tag(
    panel: *const c_char,
    socket: *const c_char,
) -> *mut c_char {
    // SAFETY: caller contract.
    match (unsafe { cstr(panel) }, unsafe { cstr(socket) }) {
        (Some(p), Some(s)) => into_c(copad_mux_proto::copad_tag(p, s)),
        _ => ptr::null_mut(),
    }
}

#[cfg(test)]
mod browser_ffi_tests {
    use super::*;

    /// Call one of the browser entry points with a JSON literal and get the
    /// response back as a parsed value, freeing the Rust allocation exactly
    /// once — the same lifecycle the Swift facade implements.
    fn call(f: unsafe extern "C" fn(*const c_char) -> *mut c_char, input: &str) -> Option<Value> {
        let arg = CString::new(input).expect("test literal has no NUL");
        // SAFETY: `arg` outlives the call; the returned pointer is freed below.
        let out = unsafe { f(arg.as_ptr()) };
        if out.is_null() {
            return None;
        }
        // SAFETY: the entry points return a pointer from `CString::into_raw`.
        let owned = unsafe { CString::from_raw(out) };
        Some(serde_json::from_slice(owned.as_bytes()).expect("response is JSON"))
    }

    #[test]
    fn canonicalize_applies_the_requested_policy() {
        let origin = call(
            copad_ffi_browser_canonicalize,
            r#"{"url":"https://github.com/o/r/pull/42","policy":"origin"}"#,
        )
        .expect("non-null");
        assert_eq!(origin["url"], "https://github.com");

        let full = call(
            copad_ffi_browser_canonicalize,
            r#"{"url":"https://github.com/o/r/pull/42","policy":"url"}"#,
        )
        .expect("non-null");
        assert_eq!(full["url"], "https://github.com/o/r/pull/42");
    }

    #[test]
    fn canonicalize_says_whether_the_title_may_be_persisted() {
        // A title carries the same exposure as a URL, and the caller must not
        // re-derive this by comparing the raw policy string (codex review C1).
        for (policy, expected) in [("origin", false), ("url", true), ("full", true)] {
            let out = call(
                copad_ffi_browser_canonicalize,
                &format!(r#"{{"url":"https://e.com/x","policy":"{policy}"}}"#),
            )
            .expect("non-null");
            assert_eq!(out["persist_title"], expected, "{policy}");
        }
        // Case and whitespace resolve HERE, so they cannot disagree with a
        // second interpretation elsewhere.
        for weird in ["ORIGIN", " origin ", "nonsense"] {
            let out = call(
                copad_ffi_browser_canonicalize,
                &format!(r#"{{"url":"https://e.com/x","policy":"{weird}"}}"#),
            )
            .expect("non-null");
            assert_eq!(out["persist_title"], false, "{weird}");
            assert_eq!(out["url"], "https://e.com", "{weird}");
        }
    }

    #[test]
    fn canonicalize_falls_back_to_origin_on_a_bad_policy() {
        // A typo must never silently widen what reaches disk.
        let out = call(
            copad_ffi_browser_canonicalize,
            r#"{"url":"https://e.com/secret/path","policy":"ful"}"#,
        )
        .expect("non-null");
        assert_eq!(out["url"], "https://e.com");
    }

    #[test]
    fn canonicalize_carries_the_backslash_and_percent_fixes_across_the_boundary() {
        // The two bugs the deleted Swift copy still had.
        let bs = call(
            copad_ffi_browser_canonicalize,
            r#"{"url":"https://example.com\\reset\\SECRET","policy":"origin"}"#,
        )
        .expect("non-null");
        assert_eq!(bs["url"], "https://example.com");

        let pct = call(
            copad_ffi_browser_canonicalize,
            r#"{"url":"https://e.com/?%63ode=SECRET&page=2","policy":"url"}"#,
        )
        .expect("non-null");
        assert_eq!(pct["url"], "https://e.com/?page=2");
    }

    #[test]
    fn normalize_repairs_an_untrusted_pane_and_reports_what_it_did() {
        let out = call(
            copad_ffi_browser_normalize,
            r#"{"pane":{"tabs":[
                {"id":"a","url":"https://e.com"},
                {"id":"..","url":"https://e.com"},
                {"id":"a","url":"https://e.com"}
            ],"active":9,"profile":"default"}}"#,
        )
        .expect("non-null");
        assert_eq!(out["pane"]["tabs"].as_array().unwrap().len(), 1);
        assert_eq!(out["pane"]["active"], 0);
        assert_eq!(out["repairs"]["dropped_invalid_id"], 1);
        assert_eq!(out["repairs"]["dropped_duplicate_id"], 1);
        assert_eq!(out["repairs"]["clean"], false);
    }

    #[test]
    fn authorize_answers_both_questions_in_one_call() {
        let read = call(
            copad_ffi_browser_authorize,
            r#"{"method":"webview.get_content","mode":"protected","profile_protected":true}"#,
        )
        .expect("non-null");
        assert_eq!(read["allowed"], false);
        assert_eq!(read["code"], "tab_protected");

        let write = call(
            copad_ffi_browser_authorize,
            r#"{"method":"webview.click","mode":"protected","profile_protected":true}"#,
        )
        .expect("non-null");
        assert_eq!(write["allowed"], true);
        assert_eq!(write["opaque_write"], true);

        let normal = call(
            copad_ffi_browser_authorize,
            r#"{"method":"webview.click","mode":"automation","profile_protected":false}"#,
        )
        .expect("non-null");
        assert_eq!(normal["allowed"], true);
        assert_eq!(normal["opaque_write"], false);
    }

    #[test]
    fn authorize_reads_an_unknown_mode_as_protected() {
        // Fail closed: a caller that cannot say what state a tab is in must not
        // be told the tab is open for automation.
        for body in [
            r#"{"method":"webview.get_content","mode":"bogus"}"#,
            r#"{"method":"webview.get_content"}"#,
        ] {
            let out = call(copad_ffi_browser_authorize, body).expect("non-null");
            assert_eq!(out["allowed"], false, "{body}");
        }
    }

    #[test]
    fn malformed_input_returns_null_with_a_diagnostic_rather_than_a_guess() {
        for f in [
            copad_ffi_browser_canonicalize as unsafe extern "C" fn(*const c_char) -> *mut c_char,
            copad_ffi_browser_normalize,
            copad_ffi_browser_authorize,
        ] {
            assert!(call(f, "{not json").is_none());
            assert!(!last_error_string().is_empty());
        }
        // A NULL pointer is a caller bug, not a parse failure — also NULL out.
        // SAFETY: passing NULL is exactly the case under test.
        assert!(unsafe { copad_ffi_browser_normalize(ptr::null()) }.is_null());
    }

    #[test]
    fn normalize_rejects_a_pane_that_is_not_a_pane() {
        assert!(call(copad_ffi_browser_normalize, r#"{"pane":42}"#).is_none());
        assert!(call(copad_ffi_browser_normalize, r#"{}"#).is_none());
    }

    // ---- history blobs ----
    //
    // These touch the filesystem through `state_dir()`, so they redirect HOME /
    // XDG_STATE_HOME and serialize on a lock, the same shape `copad-core`'s own
    // history tests use.

    static HISTORY_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_temp_state<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _guard = HISTORY_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("copad-ffi-hist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let prev_home = std::env::var("HOME").ok();
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        // SAFETY: HISTORY_ENV serializes every test that touches these vars.
        unsafe {
            std::env::set_var("HOME", &root);
            std::env::set_var("XDG_STATE_HOME", root.join("state"));
        }
        let out = f();
        // SAFETY: still holding the lock.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn a_history_blob_round_trips_through_the_boundary() {
        with_temp_state("roundtrip", || {
            let written = call(
                copad_ffi_browser_history_write,
                r#"{"tab_id":"tab-a","generation":1,"data_hex":"0001feff"}"#,
            )
            .expect("non-null");
            assert_eq!(written["ok"], true);

            let read = call(
                copad_ffi_browser_history_read,
                r#"{"tab_id":"tab-a","generation":1}"#,
            )
            .expect("non-null");
            assert_eq!(read["data_hex"], "0001feff");
        });
    }

    #[test]
    fn a_missing_or_invalid_blob_reads_as_null_so_the_caller_falls_back() {
        with_temp_state("missing", || {
            assert!(
                call(
                    copad_ffi_browser_history_read,
                    r#"{"tab_id":"tab-a","generation":7}"#
                )
                .is_none()
            );
            assert!(
                call(
                    copad_ffi_browser_history_read,
                    r#"{"tab_id":"../../etc/passwd","generation":1}"#
                )
                .is_none()
            );
        });
    }

    #[test]
    fn a_malformed_hex_payload_is_refused_rather_than_written() {
        with_temp_state("badhex", || {
            assert!(
                call(
                    copad_ffi_browser_history_write,
                    r#"{"tab_id":"tab-a","generation":1,"data_hex":"zz"}"#
                )
                .is_none()
            );
            assert!(
                call(
                    copad_ffi_browser_history_read,
                    r#"{"tab_id":"tab-a","generation":1}"#
                )
                .is_none()
            );
        });
    }

    #[test]
    fn gc_reclaims_superseded_generations_and_reports_the_count() {
        with_temp_state("gc", || {
            for generation in 1..=3 {
                call(
                    copad_ffi_browser_history_write,
                    &format!(r#"{{"tab_id":"tab-a","generation":{generation},"data_hex":"00"}}"#),
                )
                .expect("write");
            }
            let out =
                call(copad_ffi_browser_history_gc, r#"{"live":[["tab-a",3]]}"#).expect("non-null");
            assert_eq!(out["removed"], 2);
            assert!(
                call(
                    copad_ffi_browser_history_read,
                    r#"{"tab_id":"tab-a","generation":3}"#
                )
                .is_some()
            );
        });
    }

    #[test]
    fn gc_with_a_malformed_live_list_keeps_nothing_alive_but_does_not_crash() {
        with_temp_state("gc-malformed", || {
            call(
                copad_ffi_browser_history_write,
                r#"{"tab_id":"tab-a","generation":1,"data_hex":"00"}"#,
            )
            .expect("write");
            // Entries that are not [string, number] pairs are skipped, not
            // guessed at — the blob is simply unreferenced and reclaimed.
            let out = call(
                copad_ffi_browser_history_gc,
                r#"{"live":[["tab-a"],42,{"tab_id":"tab-a"}]}"#,
            )
            .expect("non-null");
            assert_eq!(out["removed"], 1);
        });
    }

    // ---- netlog ----

    #[test]
    fn netlog_records_round_trip_with_declared_coverage() {
        with_temp_state("netlog", || {
            let written = call(
                copad_ffi_browser_netlog_append,
                r#"{"panel_id":"panel1","kind":"console","record":{"ts":1,"tab_id":"t1","level":"error","text":"boom"}}"#,
            )
            .expect("non-null");
            assert_eq!(written["written"], true);

            let out = call(
                copad_ffi_browser_netlog_read,
                r#"{"panel_id":"panel1","limit":10}"#,
            )
            .expect("non-null");
            assert_eq!(out["records"].as_array().unwrap().len(), 1);
            assert_eq!(out["records"][0]["text"], "boom");
            // Every read says what the capture can and cannot see.
            assert_eq!(out["coverage"], "js+navigation");
        });
    }

    #[test]
    fn netlog_redacts_credential_headers_before_they_reach_the_file() {
        with_temp_state("netlog-redact", || {
            call(
                copad_ffi_browser_netlog_append,
                r#"{"panel_id":"panel1","kind":"net","record":{"ts":1,"tab_id":"t1","source":"script","method":"POST","url":"https://api.example.com/login","request_headers":[["Authorization","Bearer sk-live-abc"]]}}"#,
            )
            .expect("non-null");
            let out = call(
                copad_ffi_browser_netlog_read,
                r#"{"panel_id":"panel1","limit":10}"#,
            )
            .expect("non-null");
            let text = out.to_string();
            assert!(!text.contains("sk-live-abc"), "{text}");
            assert!(text.contains("redacted"), "{text}");
        });
    }

    #[test]
    fn netlog_rejects_a_record_that_does_not_match_its_kind() {
        with_temp_state("netlog-bad", || {
            assert!(
                call(
                    copad_ffi_browser_netlog_append,
                    r#"{"panel_id":"panel1","kind":"net","record":{"ts":1}}"#
                )
                .is_none()
            );
            assert!(
                call(
                    copad_ffi_browser_netlog_append,
                    r#"{"panel_id":"panel1","kind":"bogus","record":{}}"#
                )
                .is_none()
            );
        });
    }

    #[test]
    fn netlog_clear_reports_what_it_discarded() {
        with_temp_state("netlog-clear", || {
            for i in 1..=2 {
                call(
                    copad_ffi_browser_netlog_append,
                    &format!(
                        r#"{{"panel_id":"panel1","kind":"console","record":{{"ts":{i},"tab_id":"t1","level":"log","text":"x"}}}}"#
                    ),
                )
                .expect("append");
            }
            let out =
                call(copad_ffi_browser_netlog_clear, r#"{"panel_id":"panel1"}"#).expect("non-null");
            assert_eq!(out["removed"], 2);
        });
    }

    fn last_error_string() -> String {
        let p = copad_ffi_last_error();
        if p.is_null() {
            return String::new();
        }
        // SAFETY: `copad_ffi_last_error` returns a pointer to a thread-local
        // CString owned by Rust; we only read it.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}
