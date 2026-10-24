import AppKit
import CCopadFFI
import SwiftUI

/// comux's sidebar and notification center as native copad UI (decisions #169).
///
/// One `ComuxHost` per app keeps an L0 subscription to the local comux (Rust `MuxHost`
/// through copad-ffi). Every window reports which terminal panels it is SHOWING; a comux
/// client running in one of them is found by its binding tag, its TUI sidebar is claimed, and
/// the window draws spaces/agents on the left and notifications on the right instead.
/// A window whose visible panes run no comux shows nothing extra.
@MainActor
final class ComuxHost: ObservableObject {
    static let shared = ComuxHost()

    /// Decoded `MuxHost::state_json`.
    struct State: Decodable {
        var connected: Bool
        var error: String?
        var tree: L0Tree?
        var agents: L0Agents?
        var notifications: L0Notifications?
        var status: L0Status?
        var bound: [String: UInt64]
        var claimed: [UInt64]
        var overlays: [L0Overlay]
    }

    /// One comux this app hosts: the local one (key `""`) or a remote one copad opened over SSH
    /// (key `dest|command`, decisions #171).
    private final class Link {
        let handle: OpaquePointer
        var lastGen: UInt64 = 0
        init(_ h: OpaquePointer) { handle = h }
        deinit { copad_muxhost_destroy(handle) }
    }

    /// Every host's latest state, by key.
    @Published private(set) var states: [String: State] = [:]
    private var links: [String: Link] = [:]
    private var timer: Timer?
    /// window id → the terminal panels it shows.
    private var served: [ObjectIdentifier: [String]] = [:]

    /// This GUI's control socket — the `COPAD_SOCKET` every tab's shell carries, and so half of
    /// the binding tag a comux client in it reports.
    let guiSocket = "/tmp/copad-\(ProcessInfo.processInfo.processIdentifier).sock"

    private init() {
        if let h = copad_muxhost_start(nil) { links[""] = Link(h) }
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.poll() }
        }
    }

    private func poll() {
        for (key, link) in links {
            let gen = copad_muxhost_generation(link.handle)
            guard gen != link.lastGen else { continue }
            link.lastGen = gen
            guard let p = copad_muxhost_state_json(link.handle) else { continue }
            defer { copad_ffi_free_string(p) }
            states[key] = try? JSONDecoder().decode(State.self, from: Data(String(cString: p).utf8))
        }
    }

    /// The binding tag of `panel` (one definition, in Rust).
    func tag(for panel: String) -> String {
        guard let p = copad_muxhost_copad_tag(panel, guiSocket) else { return "" }
        defer { copad_ffi_free_string(p) }
        return String(cString: p)
    }

    /// A window now shows `panels` (every terminal split of its visible tab).
    func serve(window: ObjectIdentifier, panels: [String]) {
        if let local = links[""] {
            for p in panels { copad_muxhost_bind(local.handle, p, tag(for: p)) }
        }
        served[window] = panels
        pushServed()
    }

    private func pushServed() {
        let all = served.values.flatMap { $0 }
        guard let data = try? JSONEncoder().encode(all), let s = String(data: data, encoding: .utf8) else { return }
        for link in links.values { copad_muxhost_serve(link.handle, s) }
    }

    func windowClosed(_ window: ObjectIdentifier) {
        served[window] = nil
        pushServed()
    }

    func panelClosed(_ panel: String) {
        for link in links.values { copad_muxhost_unbind(link.handle, panel) }
    }

    /// The host whose comux client runs in one of `panels` with its chrome claimed.
    func key(for panels: [String]) -> String? {
        states.first { _, s in
            panels.contains { p in s.bound[p].map { s.claimed.contains($0) } ?? false }
        }?.key
    }

    /// Open a REMOTE comux in `panel`: start (or reuse) the L0 host for `dest`, bind the panel
    /// to a fresh tag, and return the command the panel must run to attach the remote client
    /// carrying that tag. `nil` = refused destination/command.
    func openRemote(dest: String, comux: String, panel: String) -> String? {
        let key = "\(dest)|\(comux)"
        if links[key] == nil {
            guard let h = copad_muxhost_start_remote(dest, comux) else { return nil }
            links[key] = Link(h)
        }
        guard let link = links[key], let t = copad_muxhost_new_remote_tag() else { return nil }
        let tag = String(cString: t)
        copad_ffi_free_string(t)
        guard let c = copad_muxhost_remote_attach_command(dest, comux, tag) else { return nil }
        defer { copad_ffi_free_string(c) }
        copad_muxhost_bind(link.handle, panel, tag)
        pushServed()
        return String(cString: c)
    }

    /// Answer a delegated overlay (comux runs the action).
    func answer(key: String, _ overlay: UInt64, choice: String?, text: String?) {
        guard let link = links[key] else { return }
        copad_muxhost_answer(link.handle, overlay, choice, text)
    }

    func command(key: String, _ req: [String: Any]) {
        guard let link = links[key], let d = try? JSONSerialization.data(withJSONObject: req),
              let s = String(data: d, encoding: .utf8) else { return }
        copad_muxhost_command(link.handle, s)
    }
}

/// What ONE window shows: the state of the comux its visible panes host, and the way to talk
/// to that comux (local or remote).
@MainActor
final class ComuxWindow: ObservableObject {
    @Published private(set) var state: ComuxHost.State?
    private(set) var key: String?

    func update(panels: [String]) {
        let host = ComuxHost.shared
        key = host.key(for: panels)
        state = key.flatMap { host.states[$0] }
    }

    func command(_ req: [String: Any]) {
        if let key { ComuxHost.shared.command(key: key, req) }
    }

    func answer(_ overlay: UInt64, choice: String?, text: String?) {
        if let key { ComuxHost.shared.answer(key: key, overlay, choice: choice, text: text) }
    }
}

/// The left panel: spaces, then every agent with its status and time in it.
struct ComuxSidebar: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void
    var toggleNotifications: () -> Void

    var body: some View {
        let s = host.state
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("comux").font(.caption.bold()).foregroundStyle(.secondary)
                Spacer()
                Button(action: toggleNotifications) {
                    HStack(spacing: 2) {
                        Image(systemName: "bell")
                        if let n = s?.notifications?.unread, n > 0 { Text("\(n)").font(.caption2.bold()) }
                    }
                }
                .buttonStyle(.borderless)
                .accessibilityIdentifier("comuxNotificationsToggle")
            }
            .padding(.horizontal, 10).padding(.vertical, 6)
            List {
                Section("spaces") {
                    ForEach(s?.tree?.sessions ?? []) { sess in
                        Button {
                            settle()
                            host.command(["cmd": "select-session", "target": sess.id])
                        } label: {
                            VStack(alignment: .leading, spacing: 1) {
                                Text(sess.name).fontWeight(sess.active ? .bold : .regular)
                                if !sess.branch.isEmpty {
                                    Text(sess.branch).font(.caption2.monospaced()).foregroundStyle(.secondary)
                                }
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("comux-space-\(sess.name)")
                    }
                }
                Section("agents") {
                    let agents = (s?.agents?.agents ?? [])
                        .sorted { ($0.status == "blocked" ? 0 : 1) < ($1.status == "blocked" ? 0 : 1) }
                    ForEach(agents) { a in
                        Button {
                            settle()
                            host.command(["cmd": "jump", "target": a.token])
                        } label: {
                            VStack(alignment: .leading, spacing: 1) {
                                HStack {
                                    Circle().fill(statusColor(a.status)).frame(width: 7, height: 7)
                                    Text("\(a.space)/\(a.title)").lineLimit(1)
                                    Spacer()
                                    Text("\(a.status) \(elapsedLabel(since: a.since_unix))")
                                        .font(.caption2.monospaced()).foregroundStyle(statusColor(a.status))
                                }
                                Text(a.detail ?? a.tool).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                            }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
            .listStyle(.sidebar)
        }
        .accessibilityIdentifier("comuxSidebar")
    }
}

/// The right panel: the notification center, newest first.
struct ComuxNotifications: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("notifications").font(.caption.bold()).foregroundStyle(.secondary)
                Spacer()
                Button("Mark all read") { host.command(["cmd": "notif-read"]) }
                    .buttonStyle(.borderless).font(.caption)
            }
            .padding(.horizontal, 10).padding(.vertical, 6)
            List(host.state?.notifications?.items ?? []) { n in
                Button {
                    settle()
                    host.command(["cmd": "notif-read", "id": n.id])
                    if !n.token.isEmpty { host.command(["cmd": "jump", "target": n.token]) }
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        HStack {
                            Image(systemName: n.kind == "blocked" ? "exclamationmark.bubble" : "checkmark.circle")
                                .foregroundStyle(n.kind == "blocked" ? .orange : .green)
                            Text("\(n.tool) · \(n.space)/\(n.tab)").fontWeight(n.read ? .regular : .bold).lineLimit(1)
                            Spacer()
                            Text(n.when).font(.caption2).foregroundStyle(.secondary)
                        }
                        Text(n.body).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .accessibilityIdentifier("comuxNotifications")
    }
}

private func statusColor(_ s: String) -> Color {
    switch s {
    case "blocked": return .orange
    case "working": return .yellow
    case "ready": return .green
    default: return .gray
    }
}

/// comux's status bar as a native strip (decisions #170): every field the TUI bar carries —
/// session, its tabs, the armed prefix, the outcome line, bells, attention, agents, unread,
/// usage, update, host — so comux can stop drawing its own.
struct ComuxStatusStrip: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void

    var body: some View {
        let s = host.state
        let st = s?.status
        let session = s?.tree?.sessions.first { $0.active }
        HStack(spacing: 8) {
            Text(session?.name ?? "").font(.caption.bold())
                .padding(.horizontal, 6).background(Capsule().fill(Color.purple.opacity(0.45)))
            ForEach(session?.tabs ?? []) { t in
                Button {
                    settle()
                    host.command(["cmd": "select-tab", "target": t.id])
                } label: {
                    HStack(spacing: 3) {
                        if t.panes.contains(where: { $0.kind == "agent" }) {
                            Circle().fill(.yellow).frame(width: 5, height: 5)
                        }
                        Text("\(t.index + 1) \(t.title)").lineLimit(1)
                        if t.bell { Text("!").bold().foregroundStyle(.red) }
                    }
                    .font(.caption.monospaced())
                    .padding(.horizontal, 6)
                    .background(Capsule().fill(t.active ? Color.accentColor.opacity(0.45) : Color.white.opacity(0.06)))
                }
                .buttonStyle(.plain)
            }
            if st?.prefix_armed == true {
                Text(st?.prefix_label ?? "^b").font(.caption.monospaced().bold())
                    .padding(.horizontal, 4).background(Capsule().fill(Color.yellow)).foregroundStyle(.black)
            }
            if let f = st?.flash {
                Text(f.text).font(.caption).foregroundStyle(f.error ? .red : .secondary).lineLimit(1)
            }
            Spacer()
            if let b = st?.bells, b > 0 { Text("! \(b)").font(.caption.bold()).foregroundStyle(.red) }
            if let a = st?.attention, a > 0 { Text("⚑\(a)").font(.caption).foregroundStyle(.orange) }
            if let n = st?.agents, n > 0 { Text("● \(n)").font(.caption).foregroundStyle(.yellow) }
            if let u = st?.unread, u > 0 { Text("✉ \(u)").font(.caption).foregroundStyle(.blue) }
            ForEach(Array((st?.usage?.windows ?? []).enumerated()), id: \.offset) { _, w in
                Text("\(w.stale ? "~" : "")\(w.provider) \(w.window) \(Int(w.pct))%")
                    .font(.caption2.monospaced())
                    .foregroundStyle(w.pct >= 90 ? .red : w.pct >= 70 ? .yellow : .green)
            }
            if let up = st?.update { Text("⬆ \(up)").font(.caption2).foregroundStyle(.green) }
            Text(Date.now, style: .time).font(.caption2.monospaced()).foregroundStyle(.secondary)
            if let h = st?.host { Text(h).font(.caption2.bold()) }
        }
        .padding(.horizontal, 8)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityIdentifier("comuxStatus")
    }
}
