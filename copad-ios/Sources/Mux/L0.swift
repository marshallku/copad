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
    let secs = max(0, Int64(now.timeIntervalSince1970) - since)
    if secs < 60 { return "\(secs)s" }
    let m = secs / 60
    if m < 60 { return "\(m)m" }
    return "\(m / 60)h\(m % 60)m"
}
