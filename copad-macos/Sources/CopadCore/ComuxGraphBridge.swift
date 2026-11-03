import Foundation

/// A message the comux work-graph page posted (decisions #204), validated. The page draws
/// another program's output (paths, prompts), so everything it sends is checked for shape and
/// bounds before copad acts on it, and a message stamped for a connection this window has since
/// left is dropped: a queued `open` from a remote comux must never run after the window
/// switched to a local one.
public enum ComuxGraphMessage: Equatable {
    case ready
    case jump(token: String)
    case open(path: String)
    case history(token: String, before: Int64?, limit: Int?)
    case mode(String)
    case agent(token: String)

    /// `body` as the page posted it; nil when it is not one of ours, is malformed, or carries
    /// another connection's `epoch`. `ready` is accepted whatever its epoch (it can predate
    /// `setHost`).
    public static func parse(_ body: Any, epoch: String) -> ComuxGraphMessage? {
        guard let d = body as? [String: Any], let type = d["type"] as? String else { return nil }
        if type == "ready" { return .ready }
        guard d["epoch"] as? String == epoch, !epoch.isEmpty else { return nil }
        switch type {
        case "jump": return token(d["token"]).map { .jump(token: $0) }
        case "agent": return token(d["token"]).map { .agent(token: $0) }
        case "mode":
            guard let m = d["mode"] as? String, m == "agent" || m == "all" else { return nil }
            return .mode(m)
        case "open":
            guard let p = d["path"] as? String, p.hasPrefix("/"), p.utf8.count <= 4096,
                  !p.unicodeScalars.contains(where: { $0.value < 0x20 }) else { return nil }
            return .open(path: p)
        case "history":
            guard let t = token(d["token"]) else { return nil }
            guard let before = number(d["before"], max: 1_000_000_000_000),
                  let limit = number(d["limit"], max: 1000) else { return nil }
            return .history(token: t, before: before.value, limit: limit.value.map { Int($0) })
        default: return nil
        }
    }

    /// A pane token: short, printable.
    private static func token(_ v: Any?) -> String? {
        guard let t = v as? String, (1...128).contains(t.utf8.count),
              t.unicodeScalars.allSatisfy({ $0.value > 0x20 && $0.value < 0x7f }) else { return nil }
        return t
    }

    /// An optional whole number in 0...max: `value` nil when absent or null; nil when bad.
    private static func number(_ v: Any?, max: Int64) -> Number? {
        guard let v, !(v is NSNull) else { return Number(value: nil) }
        guard let n = v as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID() else { return nil }
        let d = n.doubleValue
        guard d.rounded() == d, d >= 0, d <= Double(max) else { return nil }
        return Number(value: Int64(d))
    }

    /// Absent (`value` nil) or a number — kept apart from "malformed", which is no `Number`.
    private struct Number {
        let value: Int64?
    }

    /// The file to hand to the system for an `open`: the path must be absolute and resolve to
    /// a regular file (a symlink is followed, and its TARGET must be regular).
    public static func openableFile(_ path: String) -> URL? {
        guard path.hasPrefix("/") else { return nil }
        let url = URL(fileURLWithPath: path).resolvingSymlinksInPath()
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
              attrs[.type] as? FileAttributeType == .typeRegular else { return nil }
        return url
    }
}
