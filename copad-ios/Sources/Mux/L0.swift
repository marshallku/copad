import Foundation

// The comux L0 host API (copad-mux-proto, decisions #166) — the topics the native drawers,
// tab strip and status line are drawn from. Field names follow the Rust types exactly.

let l0Proto = 1

struct L0Tree: Codable, Equatable {
    var sessions: [L0Session]
    var active: String
}

struct L0Session: Codable, Equatable, Identifiable {
    var id: String
    var index: Int
    var name: String
    var active: Bool
    var branch: String
    var dir: String
    var tabs: [L0Tab]
}

extension L0Session {
    /// The TUI's subtitle: the git branch, else what the focused pane runs.
    var subtitle: String {
        if !branch.isEmpty { return branch }
        return tabs.first { $0.active }?.panes.first { $0.focused }?.label ?? " "
    }
}

struct L0Tab: Codable, Equatable, Identifiable {
    var id: String
    var index: Int
    var name: String
    var title: String
    var active: Bool
    var bell: Bool
    var panes: [L0Pane]
}

struct L0Pane: Codable, Equatable {
    var token: String
    var index: Int
    var focused: Bool
    var label: String
    var kind: String
    var status: String
}

struct L0Agents: Codable, Equatable {
    var agents: [L0Agent]
}

struct L0Agent: Codable, Equatable, Identifiable {
    var token: String
    var space: String
    var space_id: String
    var tab_id: String
    var tab_index: Int
    var title: String
    var tool: String
    var status: String
    var since_unix: Int64
    var detail: String?
    var focused: Bool

    var id: String { token }
}

struct L0Notifications: Codable, Equatable {
    var items: [L0Notification]
    var unread: Int
}

struct L0Notification: Codable, Equatable, Identifiable {
    var id: UInt64
    var when: String
    var kind: String
    var tool: String
    var space: String
    var tab: String
    var body: String
    var token: String
    var read: Bool
}

struct L0Flash: Codable, Equatable {
    var text: String
    var error: Bool
}

struct L0UsageWindow: Codable, Equatable {
    var provider: String
    var window: String
    var pct: Double
    var resets_at: Int64?
    var stale: Bool
    /// The reset as the TUI shows it (`usage_reset`), empty when off; nil from an older comux.
    var reset_text: String?
}

struct L0Status: Codable, Equatable {
    var prefix_armed: Bool
    var prefix_label: String
    var flash: L0Flash?
    var bells: Int
    var attention: Int
    var agents: Int
    var unread: Int
    var usage: L0Usage?
    var update: String?
    var host: String
}

struct L0Usage: Codable, Equatable {
    var windows: [L0UsageWindow]
    /// The TUI's readout (decisions #182): `paged` (one of `pages`, `page` on screen, rotated by
    /// comux) or `inline`; `gauge` `bar` / `text`. Absent from an older comux.
    var layout: String?
    var pages: [L0UsagePage]?
    var page: Int?
    var gauge: String?
}

/// One carousel page: `full`, `usage` or `reset`; `windows` index into `L0Usage.windows`.
struct L0UsagePage: Codable, Equatable {
    var kind: String
    var windows: [Int]
}

struct L0Clients: Codable, Equatable {
    var clients: [L0ClientInfo]
    var hidden: [String]
}

struct L0ClientInfo: Codable, Equatable {
    var id: UInt64
    var tag: String
    var cols: Int
    var rows: Int
    var claimed: [String]
}

/// A host's view of every topic. Mirrors `copad_mux_proto::Model`: a value is kept per topic
/// with its own revision, and anything not newer than what is held is ignored (delivery order
/// ACROSS topics is not monotonic).
struct L0Model: Equatable {
    var incarnation = ""
    var tree: L0Tree?
    var agents: L0Agents?
    var notifications: L0Notifications?
    var status: L0Status?
    var clients: L0Clients?
    private var revs: [String: UInt64] = [:]

    enum Applied: Equatable {
        case welcome
        case topics([String])
        case ignored
    }

    /// Fold one L0 server line (already parsed JSON).
    mutating func apply(_ m: [String: Any]) -> Applied {
        switch m["t"] as? String {
        case "welcome":
            self = L0Model()
            incarnation = m["incarnation"] as? String ?? ""
            return .welcome
        case "snapshot":
            let topics = m["topics"] as? [String: Any] ?? [:]
            var changed: [String] = []
            for (name, v) in topics {
                guard let v = v as? [String: Any] else { continue }
                let rev = (v["rev"] as? NSNumber)?.uint64Value ?? 0
                if set(name, rev: rev, data: v["data"]) { changed.append(name) }
            }
            return .topics(changed.sorted())
        case "state":
            let name = m["topic"] as? String ?? ""
            let rev = (m["rev"] as? NSNumber)?.uint64Value ?? 0
            return set(name, rev: rev, data: m["data"]) ? .topics([name]) : .ignored
        case "unavailable":
            let name = m["topic"] as? String ?? ""
            let rev = (m["rev"] as? NSNumber)?.uint64Value ?? 0
            if let held = revs[name], held >= rev { return .ignored }
            revs[name] = rev
            clear(name)
            return .topics([name])
        default:
            return .ignored
        }
    }

    private mutating func clear(_ name: String) {
        switch name {
        case "tree": tree = nil
        case "agents": agents = nil
        case "notifications": notifications = nil
        case "status": status = nil
        case "clients": clients = nil
        default: break
        }
    }

    /// Store `data` as `name` at `rev` unless something as new is held. A value that fails to
    /// decode is dropped rather than kept stale.
    private mutating func set(_ name: String, rev: UInt64, data: Any?) -> Bool {
        if let held = revs[name], held >= rev { return false }
        guard let data, let bytes = try? JSONSerialization.data(withJSONObject: data) else { return false }
        let d = JSONDecoder()
        switch name {
        case "tree": tree = try? d.decode(L0Tree.self, from: bytes)
        case "agents": agents = try? d.decode(L0Agents.self, from: bytes)
        case "notifications": notifications = try? d.decode(L0Notifications.self, from: bytes)
        case "status": status = try? d.decode(L0Status.self, from: bytes)
        case "clients": clients = try? d.decode(L0Clients.self, from: bytes)
        default: return false
        }
        revs[name] = rev
        return true
    }

    /// The active session.
    var activeSession: L0Session? { tree?.sessions.first { $0.active } }

    /// Whether `attachment` with `tag` is listed — the precondition for a claim (codex C1:
    /// the render Hello can arrive before the server registered the attachment and its tag).
    func lists(attachment: UInt64, tag: String) -> Bool {
        clients?.clients.contains { $0.id == attachment && $0.tag == tag } ?? false
    }
}

/// "4m", "2h13m" — a host renders elapsed time itself so the topic need not change each second.
func elapsedLabel(since: Int64, now: Date = Date()) -> String {
    // The TUI's `fmt_elapsed`: nothing under a minute, then whole minutes.
    let secs = max(0, Int64(now.timeIntervalSince1970) - since)
    if secs < 60 { return "" }
    let m = secs / 60
    if m < 60 { return "\(m)m" }
    return m % 60 == 0 ? "\(m / 60)h" : "\(m / 60)h\(m % 60)m"
}

// MARK: delegated overlays (decisions #170)

struct L0PickerRow: Codable, Equatable, Identifiable {
    var id: String
    var label: String
    var detail: String?
    var badge: String?
    var current: Bool?
    /// The picker's per-row actions don't apply (the resume picker's headless toggle).
    var no_actions: Bool?
    /// The pane to preview for this row, and the `agents` join key (decisions #180).
    var pane: String?
    /// The `tree` session this row stands for.
    var session: String?
}

struct L0PickerTab: Codable, Equatable {
    var name: String
    var rows: [L0PickerRow]
}

struct L0PickerAction: Codable, Equatable {
    var id: String
    var label: String
    /// The key applying it to the highlighted row (`C-r`, `d`, `Right`).
    var key: String?
    /// Answering with it keeps the overlay up (comux re-sends it under the same id).
    var stays: Bool?
}

/// A row-independent key (`Left` → `up`, `C-a` → `toggle-all`); an empty choice cancels.
struct L0PickerKey: Codable, Equatable {
    var key: String
    var choice: String?
    var stays: Bool?
}

struct L0MenuItem: Codable, Equatable, Identifiable {
    var id: String
    var label: String
    var destructive: Bool?
}

struct L0Picker: Equatable {
    var title: String
    var tabs: [L0PickerTab]
    var tab: Int
    var actions: [L0PickerAction]
    var selected: String?
    /// Rows still being gathered (a directory listing, a transcript scan): the same overlay id
    /// arrives again with them (decisions #172).
    var loading: Bool
    /// A line above the rows (an unreadable directory's error).
    var note: String
    var keys: [L0PickerKey] = []
    /// No filter field: printable keys belong to `keys`/`actions` (the center's `d`).
    var noFilter = false
}

/// A pane's screen as styled runs (`pane-preview`, decisions #180).
struct L0Preview: Codable, Equatable {
    var pane: String
    var header: String?
    var cols: Int
    var lines: [[L0Run]]
}

struct L0Run: Codable, Equatable {
    var t: String
    var fg: L0RunColor?
    var bg: L0RunColor?
    var bold: Bool?
    var dim: Bool?
    var italic: Bool?
    var underline: Bool?
    var reverse: Bool?
}

/// A palette index (the host's theme for 0–15) or `#rrggbb`.
enum L0RunColor: Codable, Equatable {
    case indexed(Int)
    case rgb(UInt32)

    init(from d: Decoder) throws {
        let c = try d.singleValueContainer()
        if let i = try? c.decode(Int.self) {
            self = .indexed(i)
            return
        }
        let s = try c.decode(String.self)
        guard s.hasPrefix("#"), s.count == 7, let v = UInt32(s.dropFirst(), radix: 16) else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "not #rrggbb: \(s)")
        }
        self = .rgb(v)
    }

    func encode(to e: Encoder) throws {
        var c = e.singleValueContainer()
        switch self {
        case let .indexed(i): try c.encode(i)
        case let .rgb(v): try c.encode(String(format: "#%06x", v))
        }
    }
}

/// A preview as a host holds it: `ok`, `loading`, `gone` (the pane closed) or `unsupported`
/// (an older comux).
struct L0PreviewState: Codable, Equatable {
    var state: String
    var preview: L0Preview?
}

/// What comux asks a host to show instead of a TUI overlay; comux keeps the meaning and runs
/// the answer itself.
enum L0OverlaySpec: Decodable, Equatable {
    case picker(L0Picker)
    case prompt(title: String, initial: String, placeholder: String)
    case confirm(message: String)
    case menu(title: String, items: [L0MenuItem])
    /// A kind this build doesn't know: answered as cancelled, never guessed at.
    case unsupported(kind: String)

    private enum K: String, CodingKey {
        case kind, title, tabs, tab, actions, initial, placeholder, message, selected, loading, note, items, keys
        case noFilter = "no_filter"
    }

    init(from d: Decoder) throws {
        let c = try d.container(keyedBy: K.self)
        let kind = try c.decode(String.self, forKey: .kind)
        switch kind {
        case "picker":
            self = .picker(L0Picker(
                title: try c.decode(String.self, forKey: .title),
                tabs: try c.decodeIfPresent([L0PickerTab].self, forKey: .tabs) ?? [],
                tab: try c.decodeIfPresent(Int.self, forKey: .tab) ?? 0,
                actions: try c.decodeIfPresent([L0PickerAction].self, forKey: .actions) ?? [],
                selected: try c.decodeIfPresent(String.self, forKey: .selected),
                loading: try c.decodeIfPresent(Bool.self, forKey: .loading) ?? false,
                note: try c.decodeIfPresent(String.self, forKey: .note) ?? "",
                keys: try c.decodeIfPresent([L0PickerKey].self, forKey: .keys) ?? [],
                noFilter: try c.decodeIfPresent(Bool.self, forKey: .noFilter) ?? false))
        case "prompt":
            self = .prompt(title: try c.decode(String.self, forKey: .title),
                           initial: try c.decodeIfPresent(String.self, forKey: .initial) ?? "",
                           placeholder: try c.decodeIfPresent(String.self, forKey: .placeholder) ?? "")
        case "confirm":
            self = .confirm(message: try c.decodeIfPresent(String.self, forKey: .message) ?? "")
        case "menu":
            self = .menu(title: try c.decodeIfPresent(String.self, forKey: .title) ?? "",
                         items: try c.decodeIfPresent([L0MenuItem].self, forKey: .items) ?? [])
        default:
            self = .unsupported(kind: kind)
        }
    }
}

/// One delegated overlay. `panel` is filled by the desktop host (the copad panel whose comux
/// client opened it); the iOS client owns its single attachment and leaves it nil.
struct L0Overlay: Decodable, Equatable, Identifiable {
    var overlay: UInt64
    var client: UInt64
    var panel: String?
    var spec: L0OverlaySpec
    var id: UInt64 { overlay }
}
