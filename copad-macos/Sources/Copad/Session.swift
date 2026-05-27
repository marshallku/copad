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
