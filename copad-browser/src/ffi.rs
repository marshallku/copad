//! The C ABI of the browser rules for Swift hosts (copad-macos, AIDE's Mac app;
//! `include/copad_browser.h`). Each entry point takes and returns one JSON object. Strings handed
//! out are `CString`s: free them with [`copad_browser_free_string`] (copad also frees them with
//! `copad_ffi_free_string`, the same allocator). A NULL return leaves its reason in
//! [`copad_browser_last_error`], which copad-ffi shares as `copad_ffi_last_error`.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::ptr;

use serde_json::{Value, json};

use crate::browser;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Records the calling thread's last error; copad-ffi reports through it too.
pub fn set_last_error<S: Into<String>>(message: S) {
    let cs = CString::new(message.into()).unwrap_or_else(|_| {
        // Impossible interior NUL; don't lose the failure signal.
        CString::new("FFI error message contained a NUL byte").unwrap()
    });
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cs));
}

pub fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Most recent error message on the calling thread, or NULL. Borrowed from a thread-local: valid
/// until the next call on the same thread, never freed by the caller.
#[unsafe(no_mangle)]
pub extern "C" fn copad_browser_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(cs) => cs.as_ptr(),
        None => ptr::null(),
    })
}

/// Frees a string a `copad_ffi_browser_*` call returned. NULL is safe.
///
/// # Safety
/// `s` must be NULL or a pointer one of this module's functions returned, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_browser_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: caller contract — it came from `CString::into_raw` here.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Sets the directory browser state lives under ([`browser::set_state_root`]). Returns 0, or -1
/// for a NULL, empty or non-UTF-8 path.
///
/// # Safety
/// `dir` must be NULL or a NUL-terminated string valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_browser_set_state_root(dir: *const c_char) -> i32 {
    if dir.is_null() {
        set_last_error("copad_browser_set_state_root: dir is NULL");
        return -1;
    }
    // SAFETY: caller contract.
    match unsafe { CStr::from_ptr(dir) }.to_str() {
        Ok(s) if !s.is_empty() => {
            browser::set_state_root(std::path::PathBuf::from(s));
            clear_last_error();
            0
        }
        _ => {
            set_last_error("copad_browser_set_state_root: dir is empty or not UTF-8");
            -1
        }
    }
}

// Browser Workbench (docs/browser-workbench-plan.md, decision #100)
//
// Deliberately only these entry points: the browser DATA types are fine for Swift to mirror, but
// the browser RULES are not, since a second implementation of a security rule can be wrong on
// its own (the `canonical_origin` bugs B1a fixed still lived in the Swift copy this replaces).
// Each takes and returns one JSON object.

/// Read a NUL-terminated UTF-8 JSON argument into a `serde_json::Value`; shared by the browser
/// entry points.
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

/// `{ url, policy }` -> `{ url, persist_title }`.
///
/// Applies `[browser] restore` to a live URL. An unknown `policy` falls back to `origin`, the
/// safe direction (matches `BrowserConfig::restore_policy`): a typo must never widen what
/// reaches disk.
///
/// `persist_title` is returned because a page title carries the same exposure as a URL, and the
/// caller must not re-derive it from the raw policy string: `"ORIGIN"`, `" origin "` and a typo
/// all resolve to origin-only HERE, and a second interpretation in Swift got that wrong.
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

/// `{ pane }` -> `{ pane, repairs }`.
///
/// Runs an untrusted `BrowserPaneSnap` through `browser::tabs::normalize`, which keeps a
/// hand-edited or imported session document from becoming filesystem authority. `repairs`
/// reports what had to be fixed.
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

/// `{ method, mode, profile_protected }` -> `{ allowed, opaque_write, code?, message? }`.
///
/// One call answers both whether this may run and whether its result must be replaced with a
/// page-independent one, so the caller can re-ask cheaply at delivery time (a read in flight
/// when a tab entered `protected` must be suppressed).
///
/// An unrecognised `mode` is treated as `protected`: a caller that cannot say what state a tab
/// is in must not be told it is open for automation.
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
        // Anything else, including a missing or misspelled value, fails closed.
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

/// `{ tab_id, generation, data_hex }` -> `{ ok }`.
///
/// Persist a tab's opaque back/forward + scroll blob as a NEW generation. The caller writes
/// `session.json` referencing it only after this returns, so a crash between the two cannot pair
/// old metadata with new history. Hex rather than base64: `copad-core` has no base64 decoder.
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

/// `{ tab_id, generation }` -> `{ data_hex }`.
///
/// NULL when the blob is absent, unreadable, oversize, or a symlink was planted at its name; the
/// caller falls back to a plain URL load rather than restoring history it cannot trust.
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

/// `{ live: [[tab_id, generation], ...] }` -> `{ removed }`.
///
/// Reclaim superseded generations after a session has committed. Only files matching the exact
/// blob grammar are candidates, so anything else in the directory survives.
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

/// `{ panel_id, kind, record, capture_bodies? }` -> `{ written }`.
///
/// `written: false` means the record was DROPPED for exceeding the per-record cap even after
/// shedding; it is reported so a caller does not treat a drop as success.
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

/// `{ panel_id, kind?, since?, tab_id?, level?, contains?, limit? }` -> `{ records, coverage }`.
///
/// `coverage` is returned on EVERY read: patching `fetch`/`XHR` is not a packet log, and an
/// agent reading an empty list must not conclude "no request was made".
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

/// `{ request, credential, live, target }` -> `{ ok }` or NULL with the reason.
///
/// The credential-fill preconditions, evaluated in Rust so there is one implementation
/// of the checks that decide whether a password is written into a page. `target` is the element
/// as observed in the SAME synchronous step that performs the injection, never an earlier
/// capture: a page can swap the input or flip its `type` without navigating.
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
        // An unrecognised mode reads as Automation here, which REFUSES the fill.
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

#[cfg(test)]
mod browser_ffi_tests {
    use super::*;

    /// Call a browser entry point with a JSON literal and parse the response, freeing the Rust
    /// allocation exactly once as the Swift facade does.
    fn call(f: unsafe extern "C" fn(*const c_char) -> *mut c_char, input: &str) -> Option<Value> {
        let arg = CString::new(input).expect("test literal has no NUL");
        let out = unsafe { f(arg.as_ptr()) };
        if out.is_null() {
            return None;
        }
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
        // A title carries the same exposure as a URL; the caller must not re-derive this from the
        // raw policy string.
        for (policy, expected) in [("origin", false), ("url", true), ("full", true)] {
            let out = call(
                copad_ffi_browser_canonicalize,
                &format!(r#"{{"url":"https://e.com/x","policy":"{policy}"}}"#),
            )
            .expect("non-null");
            assert_eq!(out["persist_title"], expected, "{policy}");
        }
        // Case and whitespace resolve HERE, not in a second interpretation elsewhere.
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
        // Fail closed.
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
        // SAFETY: passing NULL is exactly the case under test.
        assert!(unsafe { copad_ffi_browser_normalize(ptr::null()) }.is_null());
    }

    #[test]
    fn normalize_rejects_a_pane_that_is_not_a_pane() {
        assert!(call(copad_ffi_browser_normalize, r#"{"pane":42}"#).is_none());
        assert!(call(copad_ffi_browser_normalize, r#"{}"#).is_none());
    }

    fn with_temp_state<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _guard = crate::browser::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("copad-browser-ffi-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let prev_root = crate::browser::replace_state_root(Some(root.join("state")));
        let out = f();
        crate::browser::replace_state_root(prev_root);
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
            // Entries that are not [string, number] pairs are skipped; the blob is unreferenced
            // and reclaimed.
            let out = call(
                copad_ffi_browser_history_gc,
                r#"{"live":[["tab-a"],42,{"tab_id":"tab-a"}]}"#,
            )
            .expect("non-null");
            assert_eq!(out["removed"], 1);
        });
    }

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
        let p = copad_browser_last_error();
        if p.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}
