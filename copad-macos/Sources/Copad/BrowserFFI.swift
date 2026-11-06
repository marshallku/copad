import CCopadFFI
import CopadCore
import Foundation

/// C marshalling for the three browser rules in `copad-core`. Decisions such as the fallback when
/// a call fails live in `CopadCore.BrowserFFIDecode`, since `swift test` cannot reach the C
/// symbols (the linker flags are on the executable target).
///
/// The rules are not mirrored in Swift: an earlier Swift copy of the URL canonicalizer kept bugs
/// the Rust version had fixed (a backslash smuggling a path into an origin, a percent-encoded
/// `%63ode=` slipping past the token denylist).
enum BrowserFFI {
    /// Apply the `[browser] restore` policy to a live URL, and learn whether the page title may
    /// be persisted with it. Both answers come from Rust, which normalizes the policy string
    /// (`"ORIGIN"`, `" origin "`).
    ///
    /// `.unavailable` (rule not consulted) restores the blank URL-entry placeholder, never the raw
    /// URL: a failed call must not become "persist everything, tokens included".
    static func canonicalize(_ url: String, policy: String) -> BrowserCanonicalURL {
        BrowserFFIDecode.canonical(call(
            copad_ffi_browser_canonicalize,
            ["url": url, "policy": policy],
            label: "canonicalize",
        ))
    }

    /// Repair an untrusted pane snapshot read off disk. Nil means the rule could not be consulted;
    /// the caller falls back to single-URL restore.
    static func normalize(_ pane: BrowserPaneSnap) -> BrowserPaneSnap? {
        guard let encoded = try? JSONEncoder().encode(pane),
              let paneObj = try? JSONSerialization.jsonObject(with: encoded)
        else { return nil }
        return BrowserFFIDecode.normalized(call(
            copad_ffi_browser_normalize,
            ["pane": paneObj],
            label: "normalize",
        ))
    }

    /// May this browser RPC run against a tab in `mode`, and must its result be replaced with a
    /// page-independent one?
    ///
    /// Ask at dispatch, again before delivering a result, and again before any side effect that
    /// leaves the process (the `--path` screenshot write): a read in flight when a tab entered
    /// `protected` must be suppressed, and a written file cannot be unwritten.
    static func authorize(
        method: String,
        mode: String,
        profileProtected: Bool,
    ) -> BrowserAuthorization {
        BrowserFFIDecode.authorization(call(
            copad_ffi_browser_authorize,
            ["method": method, "mode": mode, "profile_protected": profileProtected],
            label: "authorize",
        ))
    }

    /// Does this restore policy keep the opaque history blob? Asked of Rust, as `persistTitle` is,
    /// because Rust normalizes the policy string (`" full "`).
    static func keepsHistory(policy: String) -> Bool {
        BrowserFFIDecode.keepsHistory(call(
            copad_ffi_browser_canonicalize,
            ["url": "", "policy": policy],
            label: "keepsHistory",
        ))
    }

    // MARK: - History blobs

    /// Persist a tab's opaque back/forward + scroll blob as a new generation. On false the caller
    /// must not record the generation in the session.
    static func writeHistory(tabID: String, generation: UInt64, data: Data) -> Bool {
        let raw = call(
            copad_ffi_browser_history_write,
            [
                "tab_id": tabID,
                "generation": generation,
                "data_hex": data.map { String(format: "%02x", $0) }.joined(),
            ],
            label: "history_write",
        )
        return BrowserFFIDecode.wroteHistory(raw)
    }

    /// Read a blob back. `nil` covers absent, unreadable, oversize and symlinked; the caller
    /// restores the plain URL.
    static func readHistory(tabID: String, generation: UInt64) -> Data? {
        BrowserFFIDecode.historyBlob(call(
            copad_ffi_browser_history_read,
            ["tab_id": tabID, "generation": generation],
            label: "history_read",
        ))
    }

    /// Reclaim superseded generations. Call after the session has committed, so the generation it
    /// references is never removed.
    @discardableResult
    static func gcHistory(live: [(String, UInt64)]) -> Int {
        BrowserFFIDecode.gcRemoved(call(
            copad_ffi_browser_history_gc,
            ["live": live.map { [$0.0, $0.1] as [Any] }],
            label: "history_gc",
        ))
    }

    // MARK: - Network + console log

    /// Append one captured record. Returns false when core dropped it for exceeding the
    /// per-record cap.
    @discardableResult
    static func appendLog(
        panelID: String,
        kind: String,
        record: [String: Any],
        captureBodies: Bool,
    ) -> Bool {
        BrowserFFIDecode.wroteLog(call(
            copad_ffi_browser_netlog_append,
            [
                "panel_id": panelID,
                "kind": kind,
                "record": record,
                "capture_bodies": captureBodies,
            ],
            label: "netlog_append",
        ))
    }

    static func readLog(panelID: String, query: [String: Any]) -> (records: [Any], coverage: String) {
        var payload = query
        payload["panel_id"] = panelID
        return BrowserFFIDecode.logRecords(call(
            copad_ffi_browser_netlog_read,
            payload,
            label: "netlog_read",
        ))
    }

    static func clearLog(panelID: String) -> Int {
        BrowserFFIDecode.gcRemoved(call(
            copad_ffi_browser_netlog_clear,
            ["panel_id": panelID],
            label: "netlog_clear",
        ))
    }

    /// Ask core whether this fill may proceed. Deliberately not re-implemented in Swift: these
    /// checks gate whether a password is written into a page, and a Swift copy once skipped core's
    /// document-generation and profile binding.
    static func validateFill(
        request: [String: Any],
        credential: CredentialRef,
        live: [String: Any],
        target: [String: Any],
    ) -> RPCError? {
        guard let data = try? JSONEncoder().encode(credential),
              let credObj = try? JSONSerialization.jsonObject(with: data)
        else {
            return RPCError(code: "invalid_params", message: "credential could not be encoded")
        }
        let verdict = BrowserFFIDecode.fillVerdict(call(
            copad_ffi_browser_validate_fill,
            ["request": request, "credential": credObj, "live": live, "target": target],
            label: "validate_fill",
        ))
        return verdict.map { RPCError(code: $0.code, message: $0.message) }
    }

    // MARK: - Credential index (metadata only)

    static func listCredentials(origin: String?) -> [CredentialRef] {
        var payload: [String: Any] = [:]
        if let origin { payload["origin"] = origin }
        return BrowserFFIDecode.credentials(call(
            copad_ffi_browser_credentials_list,
            payload,
            label: "credentials_list",
        ))
    }

    static func upsertCredential(_ ref: CredentialRef) -> Bool {
        guard let data = try? JSONEncoder().encode(ref),
              let obj = try? JSONSerialization.jsonObject(with: data)
        else { return false }
        return BrowserFFIDecode.wroteLog(call(
            copad_ffi_browser_credentials_upsert,
            ["credential": obj],
            label: "credentials_upsert",
        ).map { $0.replacingOccurrences(of: "\"ok\"", with: "\"written\"") })
    }

    static func removeCredential(id: String) -> Bool {
        BrowserFFIDecode.removedCredential(call(
            copad_ffi_browser_credentials_remove,
            ["credential_id": id],
            label: "credentials_remove",
        ))
    }

    // MARK: - Private

    /// Copy-then-free, as `CopadFFI.callJSON` does: no caller holds a raw pointer.
    /// The browser core has no state root until copad hands it one; a static `let` runs this once,
    /// thread-safely, before the first call.
    private static let stateRoot: Void = copad_ffi_browser_init()

    private static func call(
        _ fn: (UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>?,
        _ payload: [String: Any],
        label: String,
    ) -> String? {
        _ = stateRoot
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let input = String(data: data, encoding: .utf8)
        else {
            warn("\(label): could not serialize the request")
            return nil
        }
        return input.withCString { cstr -> String? in
            guard let out = fn(cstr) else {
                warn("\(label): \(CopadFFI.lastError() ?? "<no error message>")")
                return nil
            }
            defer { copad_ffi_free_string(out) }
            return String(cString: out)
        }
    }

    private static func warn(_ message: String) {
        FileHandle.standardError.write(Data("[copad-browser-ffi] \(message)\n".utf8))
    }
}
