//! C-ABI bridge from `copad_core` to platform UIs that can't link Rust directly (currently
//! `copad-macos` via SwiftPM), wrapping `TriggerEngine`.
//!
//! Strings allocated on the Rust side and returned to C must be freed with
//! `copad_ffi_free_string`; statics and thread-local error pointers must NOT. Errors are
//! reported via `copad_ffi_last_error` (thread-local).

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use copad_core::action_registry::ActionResult;
use copad_core::background::{self, BackgroundPaths};
use copad_core::event_bus::Event;
use copad_core::plugin;
use copad_core::protocol::ResponseError;
use copad_core::session::Session;
use copad_core::theme::Theme;
use copad_core::trigger::{Trigger, TriggerEngine, TriggerSink};
use serde_json::{Value, json};
use std::path::PathBuf;

// One error slot for the whole archive: the browser entry points (`copad_browser::ffi`) report
// through it too, and Swift reads every failure with `copad_ffi_last_error`.
fn set_last_error<S: Into<String>>(message: S) {
    copad_browser::ffi::set_last_error(message);
}

fn clear_last_error() {
    copad_browser::ffi::clear_last_error();
}

/// Pointer to a static NUL-terminated version string. Caller must NOT free.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_version() -> *const c_char {
    c"copad-ffi 0.1.0".as_ptr()
}

/// Echo-with-`echoed_at`-timestamp round-trip. Returns a heap-allocated JSON string (free with
/// `copad_ffi_free_string`); NULL on failure with the message in `LAST_ERROR`.
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
        // Wrap non-object input so the response is always an object.
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
    copad_browser::ffi::copad_browser_last_error()
}

// Engine FFI surface

pub struct EngineHandle {
    engine: Arc<TriggerEngine>,
    _sink: Arc<FfiSink>,
}

/// Forwards trigger action dispatch into a host-registered C callback. Fire-and-forget: returns
/// `{queued: true}`; the real result arrives via completion-event fan-out (as `LiveTriggerSink`).
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
            // No callback yet: treat as "no sink available" so the engine doesn't keep retrying.
            // `{queued:false}` in Ok avoids polluting the error path (ActionResult's Err needs a
            // code/message).
            eprintln!("[copad-ffi] dispatch_action({action}) but no Swift callback registered");
            return Ok(json!({ "queued": false, "reason": "no callback registered" }));
        };
        // CString::new fails on NUL bytes: defensive for action names, the caller's problem for
        // params.
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
        // SAFETY: the host promised to keep `user_data` alive until destroy; both CStrings live
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

/// Destroy an engine from `copad_engine_create`; call exactly once, after all in-flight FFI calls
/// into it have returned.
///
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

/// Install or replace the action callback; `callback = NULL` clears the slot.
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

/// Parse a JSON array of triggers (the `Trigger` Deserialize shape, mirroring TOML
/// `[[triggers]]`) and replace the engine's trigger set. Returns the loaded count, or -1 on
/// parse failure (see `copad_ffi_last_error`). Hot-reload semantics, including the cross-lock
/// race on await state, are documented at `TriggerEngine::set_triggers`.
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
/// `source` stamps the synthesized `Event`. Trust boundary: when synthesizing an
/// `<action>.completed` / `<action>.failed` event for await-chain promotion, `source` MUST be
/// `COMPLETION_EVENT_SOURCE` (`"copad.action"`), or `try_promote_or_drop_preflight` returns early
/// and the await state silently never advances. NULL defaults to `"macos.eventbus"`, right for
/// plain bus events but wrong for completion events.
///
/// `origin` is the tag the `[security] accept_external` gate consumes (0 = Internal, 1 =
/// External; anything else is Internal). When the macOS GUI republishes a daemon-forwarded event
/// it MUST match the wire `origin` from the bridge, or an `External` event launders into a
/// trusted local trigger.
///
/// `context_json` is a `copad_core::context::Context` snapshot (`{active_panel, active_cwd}`);
/// NULL or empty means no context, and bad JSON falls back to none rather than failing.
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
        if s.trim().is_empty() {
            None
        } else {
            // Bad JSON falls back to None: context is best-effort and `{context.X}` stays
            // literal.
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
        // 0 and any unknown value -> Internal. Refusing a malformed origin would silently drop
        // events at a protocol bump; Internal keeps local triggers firing and External opt-in.
        _ => copad_core::event_bus::Origin::Internal,
    };
    let event = Event::new(kind, source_str, payload).with_origin(origin_tag);
    let fired = h.engine.dispatch(&event, context.as_ref());
    clear_last_error();
    fired as i32
}

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

// Theme FFI surface: read-only getters over `copad_core::theme::Theme`, as serde JSON.

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

// Notify FFI surface: lets macOS's in-process `ActionRegistry` reach the same notifier the
// daemon uses, so `coctl call notify.show` works with or without the daemon.

/// Show a desktop notification via the platform notifier. `level` is 0=info (default), 1=warn,
/// 2=error; anything else is info. Returns 0 on success, 1 when no notifier exists for this
/// platform (silent no-op), -1 on validation / subprocess error (see `copad_ffi_last_error`).
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

// Plugin manifest FFI surface: validation only. Discovery varies per platform, so it stays on
// the caller side. `Activation` / `RestartPolicy` round-trip as their raw strings (custom
// Serialize impls).

/// Read `plugin.toml` at `path`, parse + validate against `copad_core::plugin::PluginManifest`.
/// Returns heap-allocated JSON (free with `copad_ffi_free_string`), or NULL on IO / parse failure
/// with the diagnostic in `LAST_ERROR`.
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

// Background FFI surface: callers pass resolved paths so each platform keeps its own conventions.

/// NULL or invalid UTF-8 -> None (bad UTF-8 is rejected quietly, per FFI convention).
unsafe fn cstr_to_pathbuf(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract — `p` is a NUL-terminated string valid for the call.
    let s = unsafe { CStr::from_ptr(p) }.to_str().ok()?;
    Some(PathBuf::from(s))
}

/// Pick a random image path from `primary_list`, falling back to `fallback_list` (NULL for none).
/// Returns a heap-allocated path to free with `copad_ffi_free_string`, or NULL when neither
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
        // mode_file only matters for is_active / toggle.
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

/// 1 if rotation mode is active, 0 if deactive, -1 on NULL / invalid UTF-8 (see
/// `copad_ffi_last_error`). A missing mode file reads as active, like Linux.
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

/// Flip the rotation mode bit and persist (creating the parent directory). Returns the new state:
/// 1 active, 0 deactive, -1 on NULL / invalid UTF-8.
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

/// Remove every line exactly equal to `entry` from the wallpaper list at `list`. Returns 1 if
/// something was removed, 0 if not present (or no list), -1 on NULL / invalid UTF-8 / IO error.
/// Backs macOS `background.delete_current`; core rewrites via temp-file + rename so a crash
/// can't truncate the catalog.
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

// Session FFI surface: argless persistence over `copad_core::session`; the path is resolved in
// core (`paths::state_dir() / "session.json"`).

/// Load the persisted session as a heap-allocated JSON string (free with
/// `copad_ffi_free_string`). NULL when no file exists, it fails to parse, the version is unknown
/// or the tab list is empty, matching `copad_core::session::load`.
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

/// Persist a session snapshot; `json` must match `copad_core::session::Session`. Returns 0 on
/// success, -1 on NULL / non-UTF-8 / JSON parse failure (see `copad_ffi_last_error`). IO errors
/// are logged by core but still return 0 (best-effort save).
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

/// Remove the persisted session file (idempotent). Always returns 0; IO failures are logged.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_session_clear() -> i32 {
    copad_core::session::clear();
    clear_last_error();
    0
}

/// Return a JSON array of built-in theme names.
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

// The comux host's C ABI (`copad_muxhost_*`) lives in `comux_host::ffi`; linking the crate keeps
// its functions in this archive for copad-macos.
pub use comux_host::ffi::MuxHostHandle;

// The browser rules' C ABI (`copad_ffi_browser_*`) lives in `copad_browser::ffi`; linking the
// crate keeps its functions in this archive for copad-macos.
pub use copad_browser::ffi::copad_browser_free_string;

/// Points the browser core at copad's state directory (`paths::state_dir()`). The Swift facade
/// calls it once before its first `copad_ffi_browser_*` call; the browser core refuses to guess a
/// root on its own.
#[unsafe(no_mangle)]
pub extern "C" fn copad_ffi_browser_init() {
    copad_browser::browser::set_state_root(copad_core::paths::state_dir());
}
