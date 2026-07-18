import CCopadFFI
import Foundation

/// Persisted tab/split layout. Wire shape mirrors `copad_core::session::Session`
/// exactly (snake_case keys, `type`-tagged `SplitSnap` variants, lowercase
/// orientation strings). Load / save / clear are owned by core and reached
/// through `copad-ffi`; this file only encodes / decodes JSON and walks
/// the in-memory tree (`leftmostCwd`).
enum Session {
    static let version: Int = 1

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

    /// Walk a SplitSnap and return the cwd of the leftmost (DFS pre-order)
    /// Terminal leaf. Used at restore time so each new panel seeds with the
    /// right cwd. Kept in Swift because callers (`TabViewController`,
    /// `PaneManager`) already hold the decoded tree and only need the leaf
    /// cwd — no FFI round-trip warranted.
    static func leftmostCwd(_ snap: SplitSnap) -> String? {
        switch snap {
        case let .terminal(cwd): cwd
        case let .branch(_, _, first, _): leftmostCwd(first)
        }
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
    /// "snake_case")]` SplitSnap enum. Manual Codable because Swift's
    /// auto-derived enum Codable picks a different on-disk layout.
    indirect enum SplitSnap: Equatable {
        case terminal(cwd: String?)
        case branch(orientation: SplitOrientation, position: Int, first: SplitSnap, second: SplitSnap)
    }
}

extension Session.SplitSnap: Codable {
    private enum CodingKeys: String, CodingKey {
        case type, cwd, orientation, position, first, second
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try c.decode(String.self, forKey: .type)
        switch kind {
        case "terminal":
            let cwd = try c.decodeIfPresent(String.self, forKey: .cwd)
            self = .terminal(cwd: cwd)
        case "branch":
            let o = try c.decode(Session.SplitOrientation.self, forKey: .orientation)
            let p = try c.decode(Int.self, forKey: .position)
            let f = try c.decode(Session.SplitSnap.self, forKey: .first)
            let s = try c.decode(Session.SplitSnap.self, forKey: .second)
            self = .branch(orientation: o, position: p, first: f, second: s)
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
        case let .terminal(cwd):
            try c.encode("terminal", forKey: .type)
            // Explicit null instead of omitted key — matches serde's
            // `Option<String>` serializer so a Swift-written snapshot
            // round-trips through core unchanged.
            try c.encode(cwd, forKey: .cwd)
        case let .branch(orientation, position, first, second):
            try c.encode("branch", forKey: .type)
            try c.encode(orientation, forKey: .orientation)
            try c.encode(position, forKey: .position)
            try c.encode(first, forKey: .first)
            try c.encode(second, forKey: .second)
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

// MARK: - v2 wire model (decision #61)

// Mirrors `copad_core::session::{SessionFileV2, WorkspaceSession, SubTab,
// PaneNode, Pane, PaneContent, LaunchProfile}`. Structs use auto Codable with
// snake_case CodingKeys; only the internally-tagged enums (`PaneNode` by
// `node`, `PaneContent` by `kind`) need manual Codable, like `SplitSnap`.
// serde accepts both an omitted key and an explicit `null` for its
// `Option` + `skip_serializing_if` fields, so Swift's optional encoding
// interoperates either way.
extension Session {
    static let versionV2: Int = 2

    /// Load the persisted session as the v2 model, migrating a v1 file forward
    /// in core (`copad_ffi_session_load_v2`). Nil on absence / parse failure.
    static func loadV2() -> SessionFileV2? {
        guard let cstr = copad_ffi_session_load_v2() else { return nil }
        defer { copad_ffi_free_string(cstr) }
        guard let data = String(cString: cstr).data(using: .utf8) else { return nil }
        do {
            return try JSONDecoder().decode(SessionFileV2.self, from: data)
        } catch {
            FileHandle.standardError.write(
                Data("[copad] session v2 decode (from core JSON) failed: \(error)\n".utf8),
            )
            return nil
        }
    }

    static func saveV2(_ file: SessionFileV2) {
        let data: Data
        do {
            data = try JSONEncoder().encode(file)
        } catch {
            FileHandle.standardError.write(
                Data("[copad] session v2 encode failed: \(error)\n".utf8),
            )
            return
        }
        let json = String(decoding: data, as: UTF8.self)
        let rc = json.withCString { copad_ffi_session_save_v2($0) }
        if rc != 0 {
            let msg = copad_ffi_last_error().map { String(cString: $0) } ?? "<unknown>"
            FileHandle.standardError.write(
                Data("[copad] session v2 save failed: \(msg)\n".utf8),
            )
        }
    }

    struct SessionFileV2: Codable, Equatable {
        let version: Int
        let sessions: [WorkspaceSession]
        let activeSessionId: String?

        private enum CodingKeys: String, CodingKey {
            case version, sessions
            case activeSessionId = "active_session_id"
        }
    }

    struct WorkspaceSession: Codable, Equatable {
        let id: String
        let name: String?
        let workspace: String?
        let subTabs: [SubTab]
        let activeSubTabId: String?

        private enum CodingKeys: String, CodingKey {
            case id, name, workspace
            case subTabs = "sub_tabs"
            case activeSubTabId = "active_sub_tab_id"
        }
    }

    struct SubTab: Codable, Equatable {
        let id: String
        let name: String?
        let root: PaneNode
        let focusedPaneId: String?

        private enum CodingKeys: String, CodingKey {
            case id, name, root
            case focusedPaneId = "focused_pane_id"
        }
    }

    /// A leaf pane: stable id + typed content.
    struct Pane: Equatable {
        let id: String
        let content: PaneContent
    }

    /// serde `#[serde(tag = "node", rename_all = "snake_case")]`.
    indirect enum PaneNode: Equatable {
        case leaf(Pane)
        case branch(orientation: SplitOrientation, ratio: Float, first: PaneNode, second: PaneNode)
    }

    /// serde `#[serde(tag = "kind", rename_all = "snake_case")]`. `nvim` is a
    /// terminal launch profile, not a kind.
    enum PaneContent: Equatable {
        case terminal(cwd: String?, launch: LaunchProfile?, tmuxRef: String?)
        case webview(url: String)
        case plugin(name: String, version: String?)
    }

    enum LaunchProfile: String, Codable, Equatable {
        case shell
        case nvim
    }
}

extension Session.PaneNode: Codable {
    private enum CodingKeys: String, CodingKey {
        case node, id, content, orientation, ratio, first, second
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .node) {
        case "leaf":
            // `Leaf(Pane)` is an internally-tagged newtype: the Pane fields
            // (id, content) sit alongside the `node` tag.
            let id = try c.decode(String.self, forKey: .id)
            let content = try c.decode(Session.PaneContent.self, forKey: .content)
            self = .leaf(Session.Pane(id: id, content: content))
        case "branch":
            self = .branch(
                orientation: try c.decode(Session.SplitOrientation.self, forKey: .orientation),
                ratio: try c.decode(Float.self, forKey: .ratio),
                first: try c.decode(Session.PaneNode.self, forKey: .first),
                second: try c.decode(Session.PaneNode.self, forKey: .second),
            )
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .node, in: c, debugDescription: "unknown PaneNode node",
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .leaf(pane):
            try c.encode("leaf", forKey: .node)
            try c.encode(pane.id, forKey: .id)
            try c.encode(pane.content, forKey: .content)
        case let .branch(orientation, ratio, first, second):
            try c.encode("branch", forKey: .node)
            try c.encode(orientation, forKey: .orientation)
            try c.encode(ratio, forKey: .ratio)
            try c.encode(first, forKey: .first)
            try c.encode(second, forKey: .second)
        }
    }
}

extension Session.PaneContent: Codable {
    private enum CodingKeys: String, CodingKey {
        case kind, cwd, launch, url, name, version
        case tmuxRef = "tmux_ref"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "terminal":
            self = .terminal(
                cwd: try c.decodeIfPresent(String.self, forKey: .cwd),
                launch: try c.decodeIfPresent(Session.LaunchProfile.self, forKey: .launch),
                tmuxRef: try c.decodeIfPresent(String.self, forKey: .tmuxRef),
            )
        case "webview":
            self = .webview(url: try c.decode(String.self, forKey: .url))
        case "plugin":
            self = .plugin(
                name: try c.decode(String.self, forKey: .name),
                version: try c.decodeIfPresent(String.self, forKey: .version),
            )
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .kind, in: c, debugDescription: "unknown PaneContent kind",
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .terminal(cwd, launch, tmuxRef):
            try c.encode("terminal", forKey: .kind)
            try c.encodeIfPresent(cwd, forKey: .cwd)
            try c.encodeIfPresent(launch, forKey: .launch)
            try c.encodeIfPresent(tmuxRef, forKey: .tmuxRef)
        case let .webview(url):
            try c.encode("webview", forKey: .kind)
            try c.encode(url, forKey: .url)
        case let .plugin(name, version):
            try c.encode("plugin", forKey: .kind)
            try c.encode(name, forKey: .name)
            try c.encodeIfPresent(version, forKey: .version)
        }
    }
}
