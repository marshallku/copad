//! The C ABI of [`crate::muxhost::MuxHost`] for Swift hosts (copad-macos, AIDE's Mac app;
//! `include/comux_host.h`). Strings handed out are `CString`s: free them with
//! [`comux_host_free_string`] (copad also frees them with `copad_ffi_free_string`, the same
//! allocator).

use std::ffi::{CStr, CString, c_char};
use std::ptr;

/// Frees a string a `copad_muxhost_*` call returned. NULL is safe.
///
/// # Safety
/// `s` must be NULL or a pointer one of this module's functions returned, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn comux_host_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: caller contract — it came from `CString::into_raw` here.
        drop(unsafe { CString::from_raw(s) });
    }
}

// comux host (decisions #169): the macOS GUI draws comux's sidebar natively. No callback crosses
// into Swift; the GUI polls `copad_muxhost_generation` and re-reads the state when it moves, so a
// window closing can never race a call into freed Swift memory.

/// Opaque handle to a running [`crate::muxhost::MuxHost`].
pub struct MuxHostHandle(crate::muxhost::MuxHost);

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
/// --stdio`. Free with `copad_muxhost_destroy`.
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
    Box::into_raw(Box::new(MuxHostHandle(crate::muxhost::MuxHost::start(ep))))
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

///
/// # Safety
/// `h` live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_state_json(h: *const MuxHostHandle) -> *mut c_char {
    // SAFETY: caller contract.
    unsafe { h.as_ref() }.map_or(ptr::null_mut(), |h| into_c(h.0.state_json().to_string()))
}

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

///
/// # Safety
/// `h` live; `choice` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_act(
    h: *const MuxHostHandle,
    overlay: u64,
    choice: *const c_char,
) {
    // SAFETY: caller contract.
    if let (Some(h), Some(c)) = (unsafe { h.as_ref() }, unsafe { cstr(choice) }) {
        h.0.act(overlay, c.to_string());
    }
}

/// Whether `panel`'s comux client asked to focus the native sidebar (`Ctrl-b e`, decisions
/// #181): 1 once per request, else 0.
///
/// # Safety
/// `h` live; `panel` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_take_focus(
    h: *const MuxHostHandle,
    panel: *const c_char,
) -> i32 {
    // SAFETY: caller contract.
    match (unsafe { h.as_ref() }, unsafe { cstr(panel) }) {
        (Some(h), Some(p)) => i32::from(h.0.take_focus_request(p)),
        _ => 0,
    }
}

/// Keep `consumer`'s preview of `pane` fresh; `pane` NULL stops it (decisions #180). The result
/// is in the state JSON under `previews.<consumer>`.
///
/// # Safety
/// `h` live; `consumer` a valid string; `pane` NULL or a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_preview(
    h: *const MuxHostHandle,
    consumer: *const c_char,
    pane: *const c_char,
    rows: u16,
    cols: u16,
) {
    // SAFETY: caller contract.
    if let (Some(h), Some(c)) = (unsafe { h.as_ref() }, unsafe { cstr(consumer) }) {
        let req = unsafe { cstr(pane) }.map(|p| crate::muxhost::PreviewReq {
            pane: p.to_string(),
            rows,
            cols,
        });
        h.0.preview(c, req);
    }
}

/// What a window's work-graph page needs for `consumer` (decisions #204). Free with
/// `copad_ffi_free_string`.
///
/// # Safety
/// `h` live; `consumer` a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_graph_json(
    h: *const MuxHostHandle,
    consumer: *const c_char,
) -> *mut c_char {
    // SAFETY: caller contract.
    match (unsafe { h.as_ref() }, unsafe { cstr(consumer) }) {
        (Some(h), Some(c)) => into_c(h.0.graph_json(c).to_string()),
        _ => ptr::null_mut(),
    }
}

/// Ask for a page of `token`'s work-graph history for `consumer`: steps before `before`
/// (negative = the newest), at most `limit` (0 or negative = comux's default).
///
/// # Safety
/// `h` live; `consumer`, `token` valid strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn copad_muxhost_graph_history(
    h: *const MuxHostHandle,
    consumer: *const c_char,
    token: *const c_char,
    before: i64,
    limit: i64,
) {
    // SAFETY: caller contract.
    if let (Some(h), Some(c), Some(t)) =
        (unsafe { h.as_ref() }, unsafe { cstr(consumer) }, unsafe {
            cstr(token)
        })
    {
        let before = u64::try_from(before).ok();
        let limit = usize::try_from(limit).ok().filter(|l| *l > 0);
        h.0.graph_history(c, t, before, limit);
    }
}

/// The work-graph page, one self-contained HTML document. Free with `copad_ffi_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn copad_graph_page() -> *mut c_char {
    into_c(copad_graph_web::page())
}

/// Ask comux to open one of its overlays (`what`: `new-session`, `resume`, `rename-session`, ...)
/// for `panel`'s client; `target` NULL = the active session/tab (decisions #172).
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

/// Start a host on a REMOTE comux (`ssh -T <dest> <comux> proxy --stdio`); NULL when the
/// destination or command is refused (see `crate::muxhost::valid_dest`).
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
    match crate::muxhost::remote_endpoint(d, c) {
        Some(ep) => Box::into_raw(Box::new(MuxHostHandle(crate::muxhost::MuxHost::start(ep)))),
        None => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn copad_muxhost_new_remote_tag() -> *mut c_char {
    into_c(crate::muxhost::new_remote_tag())
}

/// The command a copad tab runs to attach the remote comux with `tag`; NULL when refused. Free
/// with `copad_ffi_free_string`.
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
            crate::muxhost::remote_attach_command(d, c, t).map_or(ptr::null_mut(), into_c)
        }
        _ => ptr::null_mut(),
    }
}

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
