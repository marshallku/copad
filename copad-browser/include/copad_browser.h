// The browser rules' C ABI (copad-browser/src/ffi.rs, decisions #100/#210). copad-macos declares
// the copad_ffi_browser_* functions in CCopadFFI/include/copad_ffi.h; AIDE copies this file at the
// rev it pins.
//
// Call copad_browser_set_state_root before any call that touches disk (history, netlog,
// credentials): the core has no default root. copad sets it through copad_ffi_browser_init.
//
// Each copad_ffi_browser_* call takes one JSON object and returns a heap-allocated NUL-terminated
// JSON string, freed with copad_browser_free_string (copad also uses copad_ffi_free_string). NULL
// means the input was NULL, non-UTF-8 or unparseable: read copad_browser_last_error (in copad also
// copad_ffi_last_error) and treat it as the fail-closed outcome for that call (empty URL / no pane
// / refused), never as "carry on with the raw input".

#ifndef COPAD_BROWSER_H
#define COPAD_BROWSER_H

#include <stdint.h>

void copad_browser_free_string(char *s);

/// Borrowed thread-local; valid until the next call on this thread. Do not free.
const char *copad_browser_last_error(void);

/// 0 on success, -1 for a NULL, empty or non-UTF-8 path.
int32_t copad_browser_set_state_root(const char *dir);

/// `{ url, policy }` -> `{ url }`. Applies the `[browser] restore` policy.
/// An unknown policy falls back to origin-only.
char *copad_ffi_browser_canonicalize(const char *json);

/// `{ pane }` -> `{ pane, repairs }`. Repairs an untrusted BrowserPaneSnap
/// (invalid/duplicate ids, out-of-range active index, tab-count cap) and
/// reports what it had to fix.
char *copad_ffi_browser_normalize(const char *json);

/// `{ method, mode, profile_protected }` ->
/// `{ allowed, opaque_write, code?, message? }`.
/// Answers both dispatcher questions at once: may this run, and must its
/// result be replaced with a page-independent one. An unknown `mode` reads as
/// protected.
char *copad_ffi_browser_authorize(const char *json);

/// `{ tab_id, generation, data_hex }` -> `{ ok }`. Persist a tab's opaque
/// back/forward + scroll blob as a NEW generation, so the generation the
/// committed session.json still references is never overwritten in place.
char *copad_ffi_browser_history_write(const char *json);

/// `{ tab_id, generation }` -> `{ data_hex }`. NULL when the blob is absent,
/// unreadable, oversize, or a symlink was planted at its name — the caller
/// treats all of those identically and falls back to a plain URL load.
char *copad_ffi_browser_history_read(const char *json);

/// `{ live: [[tab_id, generation], ...] }` -> `{ removed }`. Reclaim
/// superseded generations after a session has committed. Only files matching
/// the exact blob grammar are candidates — not a directory wipe.
char *copad_ffi_browser_history_gc(const char *json);

/// `{ panel_id, kind, record, capture_bodies? }` -> `{ written }`. Append one
/// network or console record. `written: false` means it was DROPPED for
/// exceeding the per-record cap even after shedding — reported rather than
/// swallowed, because a caller treating a drop as success under-reports.
char *copad_ffi_browser_netlog_append(const char *json);

/// `{ panel_id, kind?, since?, tab_id?, level?, contains?, limit? }` ->
/// `{ records, coverage }`. `coverage` comes back on EVERY read: patching
/// fetch/XHR is not a packet log, and an empty list must not be read as
/// "no request was made".
char *copad_ffi_browser_netlog_read(const char *json);

/// `{ panel_id }` -> `{ removed }`.
char *copad_ffi_browser_netlog_clear(const char *json);

/// `{ origin? }` -> `{ credentials }`. The credential INDEX: metadata only.
/// No shape of this response can carry secret material — that lives in the
/// platform keychain and never crosses this boundary.
char *copad_ffi_browser_credentials_list(const char *json);

/// `{ credential }` -> `{ ok }`. Add or replace one index entry.
char *copad_ffi_browser_credentials_upsert(const char *json);

/// `{ credential_id }` -> `{ removed }`.
char *copad_ffi_browser_credentials_remove(const char *json);

/// `{ request, credential, live, target }` -> `{ ok, code?, message? }`.
/// The credential-fill preconditions, decided in Rust rather than
/// re-implemented per platform. `target` must be the element as observed in the
/// SAME synchronous step that performs the injection.
char *copad_ffi_browser_validate_fill(const char *json);

#endif
