import Foundation

// The comux RENDER protocol (copad-mux/src/proto.rs), spoken natively (decisions #168).
//
// One JSON object per line. The grammar of colours, modifiers and keys is ratatui's and
// crossterm's serde form, which comux pins with golden tests (`proto.rs` `wire_grammar`);
// this file implements exactly those strings. If a test there changes, this file changes.

/// One changed cell of a frame. `skip` marks the trailing half of a wide glyph.
struct WireCell: Decodable, Equatable {
    var x: Int
    var y: Int
    var sym: String
    var fg: String
    var bg: String
    var mods: String
    var skip: Bool

    enum CodingKeys: String, CodingKey { case x, y, sym, fg, bg, mods, skip }

    init(x: Int, y: Int, sym: String, fg: String = "Reset", bg: String = "Reset",
         mods: String = "", skip: Bool = false) {
        self.x = x; self.y = y; self.sym = sym; self.fg = fg; self.bg = bg
        self.mods = mods; self.skip = skip
    }

    init(from d: Decoder) throws {
        let c = try d.container(keyedBy: CodingKeys.self)
        x = try c.decode(Int.self, forKey: .x)
        y = try c.decode(Int.self, forKey: .y)
        sym = try c.decode(String.self, forKey: .sym)
        fg = try c.decode(String.self, forKey: .fg)
        bg = try c.decode(String.self, forKey: .bg)
        mods = try c.decode(String.self, forKey: .mods)
        skip = try c.decodeIfPresent(Bool.self, forKey: .skip) ?? false
    }
}

/// A frame: changed cells plus the cursor. `full` = a baseline repaint (clear first).
struct FrameMsg: Decodable {
    var epoch: UInt64
    var cols: Int
    var rows: Int
    var full: Bool
    var cells: [WireCell]
    var cursor: [Int]?
    var cursorVisible: Bool
    var cursorShape: Int

    enum CodingKeys: String, CodingKey {
        case epoch, cols, rows, full, cells, cursor
        case cursorVisible = "cursor_visible", cursorShape = "cursor_shape"
    }

    init(from d: Decoder) throws {
        let c = try d.container(keyedBy: CodingKeys.self)
        epoch = try c.decode(UInt64.self, forKey: .epoch)
        cols = try c.decode(Int.self, forKey: .cols)
        rows = try c.decode(Int.self, forKey: .rows)
        full = try c.decode(Bool.self, forKey: .full)
        cells = try c.decode([WireCell].self, forKey: .cells)
        cursor = try c.decodeIfPresent([Int].self, forKey: .cursor)
        cursorVisible = try c.decodeIfPresent(Bool.self, forKey: .cursorVisible) ?? true
        cursorShape = try c.decodeIfPresent(Int.self, forKey: .cursorShape) ?? 0
    }
}

/// Server → client. Unknown messages decode to `.unknown` (a newer server) and are skipped.
enum ServerMsg {
    case hello(Hello)
    case frame(FrameMsg)
    case bell
    case title(String)
    case copy(String)
    case reply(id: String, resp: [String: Any])
    case pong(UInt64)
    case refused(code: String, message: String)
    case bye
    case unknown

    struct Hello {
        var mouse: Bool
        var paste: Bool
        var incarnation: String
        var attachment: UInt64
        var commands: Bool
    }

    static func parse(_ line: Data) -> ServerMsg? {
        guard let obj = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
              let t = obj["t"] as? String else { return nil }
        switch t {
        case "hello":
            return .hello(Hello(
                mouse: obj["mouse"] as? Bool ?? false,
                paste: obj["paste"] as? Bool ?? false,
                incarnation: obj["incarnation"] as? String ?? "",
                attachment: (obj["attachment"] as? NSNumber)?.uint64Value ?? 0,
                commands: obj["commands"] as? Bool ?? false))
        case "frame":
            // Frames are the hot path: decode with Codable straight from the bytes.
            guard let f = try? JSONDecoder().decode(FrameEnvelope.self, from: line) else { return nil }
            return .frame(f.frame)
        case "bell": return .bell
        case "title": return .title(obj["text"] as? String ?? "")
        case "copy": return .copy(obj["text"] as? String ?? "")
        case "reply":
            return .reply(id: obj["id"] as? String ?? "", resp: obj["resp"] as? [String: Any] ?? [:])
        case "pong": return .pong((obj["id"] as? NSNumber)?.uint64Value ?? 0)
        case "refused":
            return .refused(code: obj["code"] as? String ?? "", message: obj["message"] as? String ?? "")
        case "bye": return .bye
        default: return .unknown
        }
    }
}

/// `{"t":"frame", …fields}` — the frame's fields sit beside the tag (serde internally tagged).
private struct FrameEnvelope: Decodable {
    var frame: FrameMsg
    init(from d: Decoder) throws { frame = try FrameMsg(from: d) }
}

/// Mouse actions comux understands (`MouseKind`, kebab-case).
enum MouseKind: String {
    case scrollUp = "scroll-up", scrollDown = "scroll-down", click, drag, up
}

/// Client → server messages, as JSON lines.
enum ClientMsg {
    case attach(cols: Int, rows: Int, expectIncarnation: String?, replaces: (UInt64, String)?)
    case env(hostTag: String)
    case key(KeyEvent)
    case paste(String)
    case mouse(x: Int, y: Int, kind: MouseKind)
    case resize(cols: Int, rows: Int)
    case command(id: String, req: [String: Any])
    case ping(UInt64)
    case detach

    var json: [String: Any] {
        switch self {
        case let .attach(cols, rows, inc, replaces):
            var m: [String: Any] = ["t": "attach", "cols": cols, "rows": rows, "heartbeat": true]
            if let inc { m["expect_incarnation"] = inc }
            if let (id, tag) = replaces { m["replaces"] = ["attachment": id, "tag": tag] }
            return m
        case let .env(tag):
            return ["t": "env", "vars": [Any](), "host_tag": tag]
        case let .key(k):
            var m = k.json
            m["t"] = "key"
            return m
        case let .paste(text): return ["t": "paste", "text": text]
        case let .mouse(x, y, kind): return ["t": "mouse", "x": x, "y": y, "kind": kind.rawValue]
        case let .resize(cols, rows): return ["t": "resize", "cols": cols, "rows": rows]
        case let .command(id, req): return ["t": "command", "id": id, "req": req]
        case let .ping(id): return ["t": "ping", "id": id]
        case .detach: return ["t": "detach"]
        }
    }

    /// One wire line, newline-terminated.
    var line: Data {
        var d = (try? JSONSerialization.data(withJSONObject: json, options: [.sortedKeys])) ?? Data()
        d.append(0x0A)
        return d
    }
}
