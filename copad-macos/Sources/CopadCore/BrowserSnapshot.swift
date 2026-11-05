import Foundation

// MARK: - Mirror types

/// Swift mirror of `copad_core::browser::tabs::BrowserPaneSnap`. Rust owns the schema; this lives
/// in `CopadCore` so the test bundle can reach it.
///
/// A schema change must update the shared fixture in the same commit. The round-trip test cannot
/// catch a future Rust `#[serde(default)]` field omitted when empty: the fixture output stays the
/// same while Swift silently drops the field on every save. No automatic guard exists.
public struct BrowserPaneSnap: Codable, Equatable, Sendable {
    public var tabs: [BrowserTabSnap]
    public var active: Int
    public var profile: String

    public init(tabs: [BrowserTabSnap], active: Int = 0, profile: String = BrowserPaneSnap.defaultProfile) {
        self.tabs = tabs
        self.active = active
        self.profile = profile
    }

    /// Mirrors `copad_core::browser::profile::DEFAULT_PROFILE`. Maps onto the platform's
    /// pre-existing data store, never a freshly identified one, or every user is signed out.
    public static let defaultProfile = "default"

    private enum CodingKeys: String, CodingKey {
        case tabs, active, profile
    }

    /// Hand-written because synthesized decoding would make `active` and `profile` required while
    /// Rust marks both `#[serde(default)]`. A pane from another producer (hand-edited file, newer
    /// Rust omitting a default) would fail to decode, `Session` would drop the pane, and restore
    /// would mint a new tab id, losing the tab's identity and history blob.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        tabs = try c.decodeIfPresent([BrowserTabSnap].self, forKey: .tabs) ?? []
        active = try c.decodeIfPresent(Int.self, forKey: .active) ?? 0
        profile = try c.decodeIfPresent(String.self, forKey: .profile)
            ?? BrowserPaneSnap.defaultProfile
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(tabs, forKey: .tabs)
        try c.encode(active, forKey: .active)
        try c.encode(profile, forKey: .profile)
    }
}

public struct BrowserTabSnap: Codable, Equatable, Sendable {
    public var id: String
    public var url: String
    public var title: String
    public var pinned: Bool
    public var historyGeneration: UInt64?
    public var historyDepth: Int?
    public var scrollY: Double?
    public var lastActive: UInt64

    public init(
        id: String,
        url: String,
        title: String = "",
        pinned: Bool = false,
        historyGeneration: UInt64? = nil,
        historyDepth: Int? = nil,
        scrollY: Double? = nil,
        lastActive: UInt64 = 0,
    ) {
        self.id = id
        self.url = url
        self.title = title
        self.pinned = pinned
        self.historyGeneration = historyGeneration
        self.historyDepth = historyDepth
        self.scrollY = scrollY
        self.lastActive = lastActive
    }

    private enum CodingKeys: String, CodingKey {
        case id, url, title, pinned
        case historyGeneration = "history_generation"
        case historyDepth = "history_depth"
        case scrollY = "scroll_y"
        case lastActive = "last_active"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        url = try c.decode(String.self, forKey: .url)
        title = try c.decodeIfPresent(String.self, forKey: .title) ?? ""
        pinned = try c.decodeIfPresent(Bool.self, forKey: .pinned) ?? false
        historyGeneration = try c.decodeIfPresent(UInt64.self, forKey: .historyGeneration)
        historyDepth = try c.decodeIfPresent(Int.self, forKey: .historyDepth)
        scrollY = try c.decodeIfPresent(Double.self, forKey: .scrollY)
        lastActive = try c.decodeIfPresent(UInt64.self, forKey: .lastActive) ?? 0
    }

    /// Omissions mirror serde's `skip_serializing_if`, so a pane with no browser state encodes
    /// byte-identically to what the older binary wrote and stays readable by it.
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(url, forKey: .url)
        if !title.isEmpty { try c.encode(title, forKey: .title) }
        if pinned { try c.encode(pinned, forKey: .pinned) }
        try c.encodeIfPresent(historyGeneration, forKey: .historyGeneration)
        try c.encodeIfPresent(historyDepth, forKey: .historyDepth)
        try c.encodeIfPresent(scrollY, forKey: .scrollY)
        try c.encode(lastActive, forKey: .lastActive)
    }
}

/// Transport for a refusal, so `CopadCore` need not know the executable's `RPCError` type.
public struct RPCErrorShape: Equatable, Sendable {
    public let code: String
    public let message: String
    public init(code: String, message: String) {
        self.code = code
        self.message = message
    }
}

/// Swift mirror of `copad_core::browser::secrets::CredentialRef`. Metadata only: it crosses the
/// RPC boundary and so is what an agent can see, and has no field that can hold secret material.
/// Adding one would undo the design; the test suite asserts it.
public struct CredentialRef: Codable, Equatable, Sendable {
    public let id: String
    public let origin: String
    public let username: String
    public let label: String?
    public let slot: String
    public let createdAt: UInt64
    public let lastUsed: UInt64?

    public init(
        id: String,
        origin: String,
        username: String,
        label: String? = nil,
        slot: String = "password",
        createdAt: UInt64 = 0,
        lastUsed: UInt64? = nil,
    ) {
        self.id = id
        self.origin = origin
        self.username = username
        self.label = label
        self.slot = slot
        self.createdAt = createdAt
        self.lastUsed = lastUsed
    }

    private enum CodingKeys: String, CodingKey {
        case id, origin, username, label, slot
        case createdAt = "created_at"
        case lastUsed = "last_used"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        origin = try c.decode(String.self, forKey: .origin)
        username = try c.decode(String.self, forKey: .username)
        label = try c.decodeIfPresent(String.self, forKey: .label)
        slot = try c.decodeIfPresent(String.self, forKey: .slot) ?? "password"
        createdAt = try c.decodeIfPresent(UInt64.self, forKey: .createdAt) ?? 0
        lastUsed = try c.decodeIfPresent(UInt64.self, forKey: .lastUsed)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(origin, forKey: .origin)
        try c.encode(username, forKey: .username)
        try c.encodeIfPresent(label, forKey: .label)
        try c.encode(slot, forKey: .slot)
        try c.encode(createdAt, forKey: .createdAt)
        try c.encodeIfPresent(lastUsed, forKey: .lastUsed)
    }

    /// Explicit rather than derived, so a field added to the type does not start crossing the
    /// boundary.
    public var wire: [String: Any] {
        var out: [String: Any] = [
            "id": id, "origin": origin, "username": username,
            "slot": slot, "created_at": createdAt,
        ]
        if let label { out["label"] = label }
        if let lastUsed { out["last_used"] = lastUsed }
        return out
    }
}

// MARK: - Snapshot lifecycle

/// How a browser pane turns itself back into a persistable snapshot. `PaneManager` rebuilds
/// `PaneContent` from the live panel on every autosave, so a restored pane must regenerate its
/// snapshot rather than carry one that goes stale on navigation.
public enum BrowserSnapshot {
    /// Which URL a snapshot should record. A pending URL (set between a navigation being asked
    /// for and committed) wins over live, as it is the newer intent:
    ///
    /// - `WKWebView.url` is nil until WebKit has one, so autosave during a restored pane's load
    ///   would persist `""` and erase the pane (seen in a real session.json).
    /// - After loading A and navigating to an unreachable B, WebKit keeps reporting A, which would
    ///   discard the navigation the user asked for.
    ///
    /// A pane whose load fails survives to be retried, as nothing clears its pending URL.
    public static func resolveURL(live: String?, pending: String?) -> String {
        if let pending, !pending.isEmpty {
            return pending
        }
        if let live, !live.isEmpty, live != "about:blank" {
            return live
        }
        return ""
    }

    /// The single-tab snapshot a pane persists before the tab strip exists. `tabID` is reused
    /// across restarts so a tab keeps its identity and future history blob.
    public static func pane(
        tabID: String,
        url: String,
        title: String = "",
        profile: String = BrowserPaneSnap.defaultProfile,
    ) -> BrowserPaneSnap {
        BrowserPaneSnap(
            tabs: [BrowserTabSnap(id: tabID, url: url, title: title)],
            active: 0,
            profile: profile,
        )
    }

    /// Every `(tab id, generation)` a pane still references; GC reclaims anything not in this set.
    /// Collect from the snapshot just committed, never the live views, or the blob the new
    /// session document points at could be deleted.
    public static func liveGenerations(_ panes: [BrowserPaneSnap]) -> [(String, UInt64)] {
        panes.flatMap { pane in
            pane.tabs.compactMap { tab in
                tab.historyGeneration.map { (tab.id, $0) }
            }
        }
    }

    /// A fresh tab id. There is deliberately no `isValidTabID` in Swift: the rule that keeps a
    /// session document from becoming a filesystem path is enforced once, in Rust, and restored
    /// panes reach a controller only through `BrowserFFI.normalize`. A second validator is the
    /// duplicated security rule this layer exists to remove. `UUID().uuidString` already
    /// qualifies; lowercased for tidiness.
    public static func freshTabID() -> String {
        UUID().uuidString.lowercased()
    }
}

// MARK: - FFI response decoding

/// Whether a browser RPC may run, and whether its result must be replaced.
public struct BrowserAuthorization: Equatable, Sendable {
    public let allowed: Bool
    public let opaqueWrite: Bool
    public let code: String?
    public let message: String?

    public init(allowed: Bool, opaqueWrite: Bool, code: String? = nil, message: String? = nil) {
        self.allowed = allowed
        self.opaqueWrite = opaqueWrite
        self.code = code
        self.message = message
    }

    /// Used when the rule could not be consulted: a dispatcher that cannot ask must not run the
    /// call, or an FFI failure would silently become "permit everything".
    public static let refused = BrowserAuthorization(
        allowed: false,
        opaqueWrite: true,
        code: "authorization_unavailable",
        message: "browser authorization could not be evaluated",
    )
}

public struct BrowserCanonicalURL: Equatable, Sendable {
    public let url: String
    /// Whether the restore policy permits persisting the page title. Decided in Rust, never
    /// re-derived from the raw policy string: `"ORIGIN"`, `" origin "` and a typo all mean
    /// origin-only there, and a Swift reading once leaked titles like "Reset password for ...".
    public let persistTitle: Bool

    public init(url: String, persistTitle: Bool) {
        self.url = url
        self.persistTitle = persistTitle
    }

    public static let unavailable = BrowserCanonicalURL(url: "", persistTitle: false)
}

/// Decoders for the three FFI responses, apart from the C marshalling because what matters is
/// which value is chosen when a call fails. Every choice is fail-closed.
public enum BrowserFFIDecode {
    /// `{ url, persist_title }` to the decision. Any failure yields `.unavailable`, never the raw
    /// input: an FFI failure must not become "persist the whole URL, tokens and all".
    public static func canonical(_ raw: String?) -> BrowserCanonicalURL {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let url = obj["url"] as? String
        else { return .unavailable }
        return BrowserCanonicalURL(
            url: url,
            // Missing reads as "do not persist".
            persistTitle: obj["persist_title"] as? Bool ?? false,
        )
    }

    /// `{ pane, repairs }` to the repaired pane; any failure is nil and the caller falls back to
    /// single-URL restore.
    public static func normalized(_ raw: String?) -> BrowserPaneSnap? {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              // Must be a dictionary: `dataWithJSONObject` raises an uncatchable ObjC exception on
              // a bare scalar top level.
              let paneObj = obj["pane"] as? [String: Any],
              let paneData = try? JSONSerialization.data(withJSONObject: paneObj)
        else { return nil }
        return try? JSONDecoder().decode(BrowserPaneSnap.self, from: paneData)
    }

    /// `{ keeps_history }`: may the opaque history blob be written? Missing reads as `false`.
    public static func keepsHistory(_ raw: String?) -> Bool {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return false }
        return obj["keeps_history"] as? Bool ?? false
    }

    /// `{ ok }`: did the blob reach disk? Fail-closed `false`, since recording a generation whose
    /// blob was never written would turn a write failure into a silent restore failure later.
    public static func wroteHistory(_ raw: String?) -> Bool {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return false }
        return obj["ok"] as? Bool ?? false
    }

    /// `{ data_hex }` to the blob; nil for every failure, which the caller treats as "restore the
    /// URL plainly".
    public static func historyBlob(_ raw: String?) -> Data? {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let hex = obj["data_hex"] as? String
        else { return nil }
        return hexDecode(hex)
    }

    /// `{ written }`: did the record reach the log? `false` means core shed it; missing reads as
    /// `false`.
    public static func wroteLog(_ raw: String?) -> Bool {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return false }
        return obj["written"] as? Bool ?? false
    }

    /// `{ records, coverage }` to the slice plus what the capture can see. A failed read yields an
    /// empty list with coverage still set, so "no records" never loses its caveat.
    public static func logRecords(_ raw: String?) -> (records: [Any], coverage: String) {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return ([], unknownCoverage) }
        return (
            obj["records"] as? [Any] ?? [],
            obj["coverage"] as? String ?? unknownCoverage
        )
    }

    /// What a read reports when coverage is unknown; never "", which an agent could read as
    /// "everything".
    public static let unknownCoverage = "unknown"

    /// `{ ok, code?, message? }` to nil when the fill may proceed, else an error. An unreadable
    /// response is a refusal: a dispatcher that cannot ask must not write a password.
    public static func fillVerdict(_ raw: String?) -> RPCErrorShape? {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let ok = obj["ok"] as? Bool
        else {
            return RPCErrorShape(
                code: "validation_unavailable",
                message: "the fill preconditions could not be evaluated",
            )
        }
        if ok { return nil }
        return RPCErrorShape(
            code: obj["code"] as? String ?? "refused",
            message: obj["message"] as? String ?? "refused by browser policy",
        )
    }

    /// `{ credentials }` to the index. An unreadable response yields an empty list, never a
    /// partial one.
    public static func credentials(_ raw: String?) -> [CredentialRef] {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let list = obj["credentials"],
              let listData = try? JSONSerialization.data(withJSONObject: list)
        else { return [] }
        return (try? JSONDecoder().decode([CredentialRef].self, from: listData)) ?? []
    }

    public static func removedCredential(_ raw: String?) -> Bool {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return false }
        return obj["removed"] as? Bool ?? false
    }

    public static func gcRemoved(_ raw: String?) -> Int {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return 0 }
        return obj["removed"] as? Int ?? 0
    }

    /// Mirrors `copad_core::browser::history::hex_decode`. Rejects odd lengths and non-hex digits
    /// so a corrupted payload surfaces as "no history", not a shorter plausible one.
    public static func hexDecode(_ hex: String) -> Data? {
        guard hex.count % 2 == 0 else { return nil }
        var out = Data(capacity: hex.count / 2)
        var high: UInt8?
        for ch in hex {
            // `isASCII` matters: `hexDigitValue` also accepts full-width and other Unicode digits,
            // which the Rust decoder rejects. The two sides must agree.
            guard ch.isASCII, let value = ch.hexDigitValue, let v = UInt8(exactly: value) else {
                return nil
            }
            if let h = high {
                out.append((h << 4) | v)
                high = nil
            } else {
                high = v
            }
        }
        return high == nil ? out : nil
    }

    /// `{ allowed, opaque_write, code?, message? }` to the decision; any failure is `.refused`.
    public static func authorization(_ raw: String?) -> BrowserAuthorization {
        guard let raw,
              let data = raw.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let allowed = obj["allowed"] as? Bool
        else { return .refused }
        return BrowserAuthorization(
            allowed: allowed,
            // Missing reads as "replace the result", the restrictive direction.
            opaqueWrite: obj["opaque_write"] as? Bool ?? true,
            code: obj["code"] as? String,
            message: obj["message"] as? String,
        )
    }
}
