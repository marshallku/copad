import CCopadFFI
import Foundation

/// Persisted tab/split layout (v3 — decision #64). Wire shape mirrors
/// `copad_core::session::Session` exactly: a flat tab list, each tab a split
/// tree of TYPED panes (`type`-tagged `SplitSnap`, `kind`-tagged `PaneContent`,
/// lowercase orientation, normalized `ratio`). Persistence is LAYOUT-ONLY:
/// terminals persist only their cwd and restore as fresh shells — process
/// persistence is the user's own tmux. Load / save / clear are owned by core and
/// reached through `copad-ffi`; this file only encodes / decodes JSON and walks
/// the in-memory tree (`leftmostCwd`).
enum Session {
    static let version: Int = 3

    /// Returns nil on absence, parse failure, version mismatch, or
    /// empty-tabs payload — core decides and logs the reason to stderr.
    static func load() -> Snapshot? {
        guard let cstr = copad_ffi_session_load() else { return nil }
        defer { copad_ffi_free_string(cstr) }
        let json = String(cString: cstr)
        guard let data = json.data(using: .utf8) else { return nil }
        do {
            return try JSONDecoder().decode(Snapshot.self, from: data)
        } catch {
            // Should not happen — core just produced this JSON. Surfacing
            // here means Swift's Codable and serde's `Session` drifted.
            FileHandle.standardError.write(
                Data("[copad] session decode (from core JSON) failed: \(error)\n".utf8),
            )
            return nil
        }
    }

    static func save(_ snap: Snapshot) {
        let encoder = JSONEncoder()
        let data: Data
        do {
            data = try encoder.encode(snap)
        } catch {
            FileHandle.standardError.write(
                Data("[copad] session encode failed: \(error)\n".utf8),
            )
            return
        }
        let json = String(decoding: data, as: UTF8.self)
        let rc = json.withCString { copad_ffi_session_save($0) }
        if rc != 0 {
            let msg = copad_ffi_last_error().map { String(cString: $0) } ?? "<unknown>"
            FileHandle.standardError.write(
                Data("[copad] session save failed: \(msg)\n".utf8),
            )
        }
    }

    static func clear() {
        _ = copad_ffi_session_clear()
    }
}

// MARK: - Wire model

//
// PaneManager / TabViewController construct + consume these types in-
// process. The FFI surface only ever crosses serialized JSON, so the
// Swift types stay as the canonical in-memory shape for the rest of the
// macOS codebase.

extension Session {
    struct Snapshot: Codable, Equatable {
        let version: Int
        let tabs: [TabSnap]
        let currentTab: Int

        private enum CodingKeys: String, CodingKey {
            case version
            case tabs
            case currentTab = "current_tab"
        }
    }

    struct TabSnap: Codable, Equatable {
        let customTitle: String?
        let root: SplitSnap

        private enum CodingKeys: String, CodingKey {
            case customTitle = "custom_title"
            case root
        }
    }

    /// Wire-compatible with serde's `#[serde(tag = "type", rename_all =
    /// "snake_case")]` SplitSnap enum. A leaf carries typed `PaneContent`; a
    /// branch carries a normalized `ratio` (0..1). Manual Codable because
    /// Swift's auto-derived enum Codable picks a different on-disk layout.
    indirect enum SplitSnap: Equatable {
        case leaf(content: PaneContent)
        case branch(orientation: SplitOrientation, ratio: Float, first: SplitSnap, second: SplitSnap)
    }

    /// Wire-compatible with serde's `#[serde(tag = "kind", rename_all =
    /// "snake_case")]` PaneContent enum. Terminals persist only their cwd.
    enum PaneContent: Equatable {
        case terminal(cwd: String?)
        case webview(url: String)
        case plugin(name: String, panelName: String?, version: String?)
        case cockpit
    }
}

extension Session.SplitSnap: Codable {
    private enum CodingKeys: String, CodingKey {
        case type, content, orientation, ratio, first, second
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try c.decode(String.self, forKey: .type)
        switch kind {
        case "leaf":
            self = .leaf(content: try c.decode(Session.PaneContent.self, forKey: .content))
        case "branch":
            let o = try c.decode(Session.SplitOrientation.self, forKey: .orientation)
            let r = try c.decode(Float.self, forKey: .ratio)
            let f = try c.decode(Session.SplitSnap.self, forKey: .first)
            let s = try c.decode(Session.SplitSnap.self, forKey: .second)
            self = .branch(orientation: o, ratio: r, first: f, second: s)
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type, in: c,
                debugDescription: "unknown SplitSnap type: \(kind)",
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .leaf(content):
            try c.encode("leaf", forKey: .type)
            try c.encode(content, forKey: .content)
        case let .branch(orientation, ratio, first, second):
            try c.encode("branch", forKey: .type)
            try c.encode(orientation, forKey: .orientation)
            try c.encode(ratio, forKey: .ratio)
            try c.encode(first, forKey: .first)
            try c.encode(second, forKey: .second)
        }
    }
}

extension Session.PaneContent: Codable {
    private enum CodingKeys: String, CodingKey {
        case kind, cwd, url, name, version
        case panelName = "panel_name"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "terminal":
            self = .terminal(cwd: try c.decodeIfPresent(String.self, forKey: .cwd))
        case "webview":
            self = .webview(url: try c.decode(String.self, forKey: .url))
        case "plugin":
            self = .plugin(
                name: try c.decode(String.self, forKey: .name),
                panelName: try c.decodeIfPresent(String.self, forKey: .panelName),
                version: try c.decodeIfPresent(String.self, forKey: .version),
            )
        case "cockpit":
            self = .cockpit
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .kind, in: c, debugDescription: "unknown PaneContent kind",
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .terminal(cwd):
            try c.encode("terminal", forKey: .kind)
            // Omit when nil to match serde's `skip_serializing_if = "Option::is_none"`.
            try c.encodeIfPresent(cwd, forKey: .cwd)
        case let .webview(url):
            try c.encode("webview", forKey: .kind)
            try c.encode(url, forKey: .url)
        case let .plugin(name, panelName, version):
            try c.encode("plugin", forKey: .kind)
            try c.encode(name, forKey: .name)
            try c.encodeIfPresent(panelName, forKey: .panelName)
            try c.encodeIfPresent(version, forKey: .version)
        case .cockpit:
            try c.encode("cockpit", forKey: .kind)
        }
    }
}

extension Session {
    /// Wire-facing orientation (the one in `SplitNode.swift` is
    /// renderer-facing). Both share the `"horizontal"`/`"vertical"`
    /// strings serde's `rename_all="lowercase"` produces.
    enum SplitOrientation: String, Codable, Equatable {
        case horizontal
        case vertical
    }
}
