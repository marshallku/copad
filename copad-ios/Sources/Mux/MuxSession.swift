import Combine
import Foundation
import NIOSSH
import SwiftUI

/// One live connection to a comux server: an L0 channel (lists, status, notifications) and a
/// render channel (the composed screen + input), over ONE SSH connection.
///
/// The contract, from the plan's codex rounds (§ iOS native client):
/// * L0 first; the render attach names the incarnation L0 saw, so a restart in between is
///   refused before it can resize the shared view.
/// * Chrome is claimed only once the `clients` topic lists our attachment WITH our tag (the
///   render Hello can arrive before the server registered either).
/// * Input is enabled only once there is an attachment, a full frame and a snapshot.
/// * Navigation goes over the RENDER channel as an ordered Command, after any IME text.
/// * A channel silent past `deadline` (no pong) tears the whole session down; nothing typed
///   meanwhile is replayed.
@MainActor
final class MuxSession: ObservableObject, KeySink {
    enum Phase: Equatable {
        case idle
        case connecting(String)
        case live
        case failed(String)
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var model = L0Model()
    @Published private(set) var title = ""
    /// The latest outcome line (`flash`), shown briefly by the status strip.
    @Published private(set) var flash: L0Flash?
    /// The newest notification announced since connecting (for an in-app banner).
    @Published private(set) var lastNotify: L0Notification?
    /// Overlays comux handed this app (decisions #170), oldest first; the first is shown.
    @Published private(set) var overlays: [L0Overlay] = []
    /// Unknown host key awaiting the user's confirmation (TOFU).
    @Published var pendingHostKey: String?

    /// The mirror grid. Not @Published: the terminal view redraws on `onGrid`, capped to the
    /// display — every frame is still APPLIED here (codex C3).
    private(set) var grid = Grid()
    var onGrid: (() -> Void)?
    var onBell: (() -> Void)?
    var onCopy: ((String) -> Void)?

    private(set) var host: MuxHost
    private var ssh: SSHConnection?
    private var l0: ExecChannel?
    private var render: ExecChannel?
    private var attachment: UInt64?
    private var claimed = false
    private var gotFull = false
    /// Last line per channel: each has its own deadline, so one channel's pongs cannot
    /// keep the other alive.
    private var lastL0 = Date()
    private var lastRender = Date()
    private var pingTimer: Timer?
    private var nextCmd = 1
    private var replies: [String: ([String: Any]) -> Void] = [:]
    private var size = (cols: 80, rows: 24)
    /// Bumped per connection, so callbacks from a torn-down one are ignored.
    private var generation = 0
    /// The attachment to replace on the next connect (the phone reconnecting).
    private var previous: UInt64?

    static let deadline: TimeInterval = 30
    static let features = ["sidebar", "status", "overlays"]

    /// This install's binding tag: how the host finds our attachment in `clients`.
    static var tag: String {
        let k = "copad.installTag"
        if let t = UserDefaults.standard.string(forKey: k) { return t }
        let t = "ios:" + UUID().uuidString.lowercased()
        UserDefaults.standard.set(t, forKey: k)
        return t
    }

    init(host: MuxHost) {
        self.host = host
        // A token that arrives (or changes) while connected is registered at once (codex C4).
        pushSub = PushRegistry.shared.$token.sink { [weak self] _ in
            Task { @MainActor in self?.whenReady() }
        }
    }

    private var pushSub: AnyCancellable?
    /// The token this connection last registered with comux.
    private var pushSent: String?
    /// A tapped notification's pane, jumped to once the session is ready.
    var pendingJump: String?

    /// Once input works: register for pushes, and take a pending notification jump.
    private func whenReady() {
        guard inputEnabled else { return }
        PushRegistry.shared.enable()
        if let pane = pendingJump {
            pendingJump = nil
            if !pane.isEmpty { jump(pane) }
        }
        guard let token = PushRegistry.shared.token, token != pushSent else { return }
        pushSent = token
        let hostID = host.id.uuidString
        command(["cmd": "push-register", "token": token, "env": PushRegistry.environment,
                 "topic": Bundle.main.bundleIdentifier ?? "", "host": hostID,
                 "device": UIDevice.current.name]) { resp in
            let ok = resp["ok"] as? Bool ?? false
            PushRegistry.shared.setStatus(host: hostID, ok ? nil : (resp["error"] as? String ?? "push refused"))
        }
    }

    var inputEnabled: Bool { phase == .live && gotFull && attachment != nil && model.tree != nil }

    // MARK: lifecycle

    func connect() {
        teardown(reason: nil)
        generation += 1
        let gen = generation
        phase = .connecting("Connecting to \(host.host)…")
        Task {
            do {
                try await open(gen)
            } catch let SSHError.unknownHostKey(k) {
                guard gen == generation else { return }
                pendingHostKey = k
                fail(gen, "Confirm the host key to continue.")
            } catch {
                fail(gen, error.localizedDescription)
            }
        }
    }

    /// The user confirmed the host key shown in `pendingHostKey`.
    func trustPendingKey() -> MuxHost? {
        guard let k = pendingHostKey else { return nil }
        host.hostKey = k
        pendingHostKey = nil
        return host
    }

    /// App backgrounded: say goodbye (best effort) and drop everything.
    func suspend() {
        if let r = render { r.write(ClientMsg.detach.line) }
        previous = attachment
        generation += 1 // the closing channels' callbacks must not turn .idle into .failed
        teardown(reason: nil)
        phase = .idle
    }

    private func open(_ gen: Int) async throws {
        let key = try DeviceKey.nioKey()
        let conn = try await SSHConnection.connect(host: host.host, port: host.port, user: host.user,
                                                   key: key, knownKey: host.hostKey)
        guard gen == generation else { conn.close(); return }
        ssh = conn

        // L0 first.
        var l0Lines = LineSplitter()
        let l0ch = try await conn.exec(host.command, onData: { [weak self] d in
            let lines = l0Lines.feed(d)
            Task { @MainActor in self?.onL0(gen, lines) }
        }, onClose: { [weak self] why in
            Task { @MainActor in self?.fail(gen, "Lost the list channel: \(why)") }
        })
        guard gen == generation else { return }
        l0 = l0ch
        l0ch.write(Self.line(["t": "subscribe", "v": l0Proto]))
        phase = .connecting("Waiting for comux…")
        lastL0 = Date()
        lastRender = Date()
        startPing(gen)
    }

    private func openRender(_ gen: Int) async {
        guard let conn = ssh, render == nil else { return }
        var lines = LineSplitter()
        do {
            let ch = try await conn.exec(host.command, onData: { [weak self] d in
                // Parse on the NIO thread: frames are the hot path.
                let msgs = lines.feed(d).compactMap(ServerMsg.parse)
                Task { @MainActor in self?.onRender(gen, msgs) }
            }, onClose: { [weak self] why in
                Task { @MainActor in self?.fail(gen, "Lost the screen channel: \(why)") }
            })
            guard gen == generation else { ch.close(); return }
            render = ch
            lastRender = Date()
            let replaces = previous.map { ($0, Self.tag) }
            ch.write(ClientMsg.attach(cols: size.cols, rows: size.rows,
                                      expectIncarnation: model.incarnation, replaces: replaces).line)
        } catch {
            fail(gen, error.localizedDescription)
        }
    }

    /// Answer the overlay comux handed us: `choice` for a picker/confirm, `text` for a prompt,
    /// both nil to cancel. comux runs the action itself.
    func answerOverlay(_ id: UInt64, choice: String?, text: String?) {
        overlays.removeAll { $0.overlay == id }
        var m: [String: Any] = ["t": "overlay-result", "overlay": id]
        if let choice { m["choice"] = choice }
        if let text { m["text"] = text }
        l0?.write(Self.line(m))
    }

    /// Ask comux to open one of its overlays for our attachment (`new-session`, `resume`,
    /// `rename-session`, `kill-session`, `rename-tab`, `close-tab`, …; decisions #172). It
    /// arrives as an ordinary delegated overlay.
    func openOverlay(_ what: String, target: String? = nil) {
        guard let attachment, claimed else { return }
        var m: [String: Any] = ["t": "open", "attachment": attachment, "what": what]
        if let target { m["target"] = target }
        l0?.write(Self.line(m))
    }

    private func teardown(reason: String?) {
        pingTimer?.invalidate(); pingTimer = nil
        l0?.close(); render?.close(); ssh?.close()
        l0 = nil; render = nil; ssh = nil
        attachment = nil; claimed = false; gotFull = false; pushSent = nil
        overlays.removeAll()
        replies.removeAll()
        grid.reset()
        onGrid?()
        if let reason { phase = .failed(reason) }
    }

    private func fail(_ gen: Int, _ why: String) {
        guard gen == generation else { return }
        generation += 1 // late callbacks from this connection are now ignored
        teardown(reason: why)
    }

    private func startPing(_ gen: Int) {
        var n: UInt64 = 0
        pingTimer = Timer.scheduledTimer(withTimeInterval: 10, repeats: true) { [weak self] _ in
            Task { @MainActor in
                guard let self, gen == self.generation else { return }
                let now = Date()
                let renderSilent = self.render != nil && now.timeIntervalSince(self.lastRender) > Self.deadline
                if now.timeIntervalSince(self.lastL0) > Self.deadline || renderSilent {
                    self.fail(gen, "comux stopped answering.")
                    return
                }
                n += 1
                self.l0?.write(Self.line(["t": "ping", "id": n]))
                self.render?.write(ClientMsg.ping(n).line)
            }
        }
    }

    // MARK: L0

    private func onL0(_ gen: Int, _ lines: [Data]) {
        guard gen == generation else { return }
        for line in lines {
            guard let m = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] else {
                if model.incarnation.isEmpty {
                    fail(gen, "The host printed something before comux started: "
                         + String(decoding: line.prefix(120), as: UTF8.self))
                }
                continue
            }
            lastL0 = Date()
            switch m["t"] as? String {
            case "welcome":
                let proto = (m["proto"] as? NSNumber)?.intValue ?? 0
                guard proto == l0Proto else {
                    fail(gen, "comux speaks L0 protocol \(proto); this app speaks \(l0Proto). Update the older side.")
                    return
                }
                _ = model.apply(m)
            case "snapshot":
                _ = model.apply(m)
                if render == nil { Task { await openRender(gen) } }
                whenReady()
            case "state", "unavailable":
                _ = model.apply(m)
                maybeClaim()
            case "event":
                let kind = m["kind"] as? String
                let data = (m["data"] as? [String: Any]).flatMap { try? JSONSerialization.data(withJSONObject: $0) }
                if kind == "flash", let data, let f = try? JSONDecoder().decode(L0Flash.self, from: data) {
                    flash = f
                } else if kind == "notify", let data,
                          let n = try? JSONDecoder().decode(L0Notification.self, from: data) {
                    lastNotify = n
                }
            case "overlay":
                guard let data = try? JSONSerialization.data(withJSONObject: m),
                      let o = try? JSONDecoder().decode(L0Overlay.self, from: data),
                      o.client == attachment else { break }
                // The same id again = its rows landed (decisions #172): replace in place.
                if let i = overlays.firstIndex(where: { $0.overlay == o.overlay }) {
                    overlays[i] = o
                } else {
                    overlays.append(o)
                }
            case "overlay-closed":
                let id = (m["overlay"] as? NSNumber)?.uint64Value
                overlays.removeAll { $0.overlay == id }
            case "error":
                // A refused claim: the TUI chrome simply stays. Not fatal.
                claimed = false
            default:
                break
            }
        }
    }

    /// Claim the chrome once `clients` lists our attachment with our tag (codex C1).
    private func maybeClaim() {
        guard !claimed, let a = attachment, model.lists(attachment: a, tag: Self.tag) else { return }
        claimed = true
        l0?.write(Self.line(["t": "host-ready", "attachment": a,
                             "incarnation": model.incarnation, "features": Self.features]))
    }

    // MARK: render

    private func onRender(_ gen: Int, _ msgs: [ServerMsg]) {
        guard gen == generation else { return }
        var changed = false
        for msg in msgs {
            lastRender = Date()
            switch msg {
            case let .hello(h):
                guard h.commands else {
                    fail(gen, "This comux is too old for the app (no render commands). Update comux.")
                    return
                }
                attachment = h.attachment
                previous = nil
                render?.write(ClientMsg.env(hostTag: Self.tag).line)
                maybeClaim()
            case let .frame(f):
                changed = grid.apply(f) || changed
                if f.full, !gotFull {
                    gotFull = true
                    phase = .live
                    whenReady()
                }
            case .bell: onBell?()
            case let .title(t): title = t
            case let .copy(t): onCopy?(t)
            case let .reply(id, resp): replies.removeValue(forKey: id)?(resp)
            case let .refused(_, message):
                // The server restarted between our two channels: start over cleanly.
                fail(gen, "comux restarted (\(message)). Reconnect.")
                return
            case .bye:
                fail(gen, "Detached by comux.")
                return
            case .pong, .unknown: break
            }
        }
        if changed { onGrid?() }
    }

    // MARK: input

    func send(_ msg: ClientMsg) {
        guard inputEnabled else { return }
        render?.write(msg.line)
    }

    func sendText(_ text: String) {
        for k in KeyEvent.keys(for: text) { send(.key(k)) }
    }

    func sendKey(_ k: KeyEvent) { send(.key(k)) }

    func resize(cols: Int, rows: Int) {
        let cols = max(cols, 10), rows = max(rows, 4)
        guard (cols, rows) != size else { return }
        size = (cols, rows)
        if attachment != nil { render?.write(ClientMsg.resize(cols: cols, rows: rows).line) }
    }

    /// Run a control request on the render channel, ordered after everything typed so far.
    /// The caller commits (or cancels) any IME composition FIRST (codex C2).
    func command(_ req: [String: Any], done: (([String: Any]) -> Void)? = nil) {
        guard inputEnabled, let r = render else { return }
        let id = "i\(nextCmd)"
        nextCmd += 1
        if let done { replies[id] = done }
        r.write(ClientMsg.command(id: id, req: req).line)
    }

    func selectSession(_ id: String) { command(["cmd": "select-session", "target": id]) }
    func selectTab(_ id: String) { command(["cmd": "select-tab", "target": id]) }
    func jump(_ token: String) { command(["cmd": "jump", "target": token]) }
    func newTab() { command(["cmd": "new-tab"]) }
    func markRead(_ id: UInt64?) {
        var req: [String: Any] = ["cmd": "notif-read"]
        if let id { req["id"] = id }
        command(req)
    }

    private static func line(_ obj: [String: Any]) -> Data {
        var d = (try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys])) ?? Data()
        d.append(0x0A)
        return d
    }
}
