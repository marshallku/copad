import Foundation

/// Persisted tab/split layout. Port of `nestty-linux/src/session.rs`:
/// same JSON wire shape (snake_case keys, `type`-tagged SplitSnap
/// variants, `lowercase` orientation strings), same `state_dir()`
/// resolution (macOS = `~/Library/Application Support/nestty/`,
/// matches `nestty_core::paths::state_dir()`), same versioning
/// contract — unknown versions are rejected instead of best-effort
/// parsed so a future schema does not produce a half-restored state.
enum Session {
    static let version: Int = 1

    /// Path duplicated from `nestty_core::paths::state_dir()`'s macOS
    /// branch. If that branch changes upstream this file lands at the
    /// wrong path; keep the two in lock-step (same notes as
    /// `NesttyConfig.configPath()`).
    static func filePath() -> URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appending(path: "Library/Application Support/nestty/session.json")
    }

    /// Read + decode + version-check. nil on absence, parse failure,
    /// version mismatch, or empty-tabs payload. Logs failures to
    /// stderr so the user sees why a restore was skipped.
    static func load() -> Snapshot? {
        let path = filePath()
        guard let data = try? Data(contentsOf: path) else { return nil }
        let decoder = JSONDecoder()
        let snap: Snapshot
        do {
            snap = try decoder.decode(Snapshot.self, from: data)
        } catch {
            FileHandle.standardError.write(
                Data("[nestty] session parse failed: \(error)\n".utf8),
            )
            return nil
        }
        guard snap.version == version else {
            FileHandle.standardError.write(
                Data("[nestty] session version mismatch (file=\(snap.version), expected=\(version)) — ignoring\n".utf8),
            )
            return nil
        }
        if snap.tabs.isEmpty { return nil }
        return snap
    }

    /// Atomic write via `.tmp` rename so a crash mid-write doesn't
    /// leave a truncated session.json on disk.
    static func save(_ snap: Snapshot) {
        let path = filePath()
        let parent = path.deletingLastPathComponent()
        do {
            try FileManager.default.createDirectory(
                at: parent, withIntermediateDirectories: true,
            )
        } catch {
            FileHandle.standardError.write(
                Data("[nestty] session save: mkdir \(parent.path) failed: \(error)\n".utf8),
            )
            return
        }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data: Data
        do { data = try encoder.encode(snap) } catch {
            FileHandle.standardError.write(
                Data("[nestty] session serialize failed: \(error)\n".utf8),
            )
            return
        }
        // `Data.write(.atomic)` writes to a sibling temp under the same
        // parent and renames over the final path — atomic from any
        // concurrent reader's perspective, and works whether the final
        // path already exists or not. (Earlier draft layered a
        // `replaceItemAt` on top, but that helper REQUIRES the
        // destination to pre-exist, so it blew up on first launch /
        // after `Session.clear()`.)
        do {
            try data.write(to: path, options: .atomic)
        } catch {
            FileHandle.standardError.write(
                Data("[nestty] session write \(path.path) failed: \(error)\n".utf8),
            )
        }
    }

    /// Remove the persisted session file. Called on close when the
    /// snapshot would be empty — keeping a stale file would surface
    /// vanished tabs on the next launch.
    static func clear() {
        let path = filePath()
        do {
            try FileManager.default.removeItem(at: path)
        } catch CocoaError.fileNoSuchFile, CocoaError.fileReadNoSuchFile {
            // Idempotent: nothing to clear.
        } catch {
            FileHandle.standardError.write(
                Data("[nestty] session clear failed: \(error)\n".utf8),
            )
        }
    }

    /// Walk a SplitSnap and return the cwd of the leftmost (DFS pre-
    /// order) Terminal leaf. Mirrors Linux's `leftmost_cwd` — used at
    /// restore-time so each new panel seeds with the right cwd.
    static func leftmostCwd(_ snap: SplitSnap) -> String? {
        switch snap {
        case let .terminal(cwd): cwd
        case let .branch(_, _, first, _): leftmostCwd(first)
        }
    }
}

// MARK: - Wire model

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

    /// Wire-compatible with Linux's `serde(tag = "type", rename_all =
    /// "snake_case")` SplitSnap enum. Manual Codable: Swift's auto-
    /// derived Codable for an enum with associated values uses a
    /// different on-disk layout (and isn't `tag`-flat to begin with).
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
            // Encode null explicitly when absent so the wire shape
            // matches Linux's `Option<String>` serializer (which emits
            // `"cwd": null` rather than dropping the key).
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
    /// Mirror of Linux's `SplitOrientation` enum (the Swift one in
    /// `SplitNode.swift` is renderer-facing; this one is wire-facing
    /// and shares the `"horizontal"`/`"vertical"` rename_all=lowercase
    /// strings).
    enum SplitOrientation: String, Codable, Equatable {
        case horizontal
        case vertical
    }
}
