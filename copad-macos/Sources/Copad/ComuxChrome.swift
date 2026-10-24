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
    }

    @Published private(set) var state: State?
    private var handle: OpaquePointer?
    private var lastGen: UInt64 = 0
    private var timer: Timer?
    /// window id → the terminal panels it shows.
    private var served: [ObjectIdentifier: [String]] = [:]

    /// This GUI's control socket — the `COPAD_SOCKET` every tab's shell carries, and so half of
    /// the binding tag a comux client in it reports.
    let guiSocket = "/tmp/copad-\(ProcessInfo.processInfo.processIdentifier).sock"

    private init() {
        handle = copad_muxhost_start(nil)
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.poll() }
        }
    }

    private func poll() {
        guard let h = handle else { return }
        let gen = copad_muxhost_generation(h)
        guard gen != lastGen else { return }
        lastGen = gen
        guard let p = copad_muxhost_state_json(h) else { return }
        defer { copad_ffi_free_string(p) }
        state = try? JSONDecoder().decode(State.self, from: Data(String(cString: p).utf8))
    }

    /// The binding tag of `panel` (one definition, in Rust).
    func tag(for panel: String) -> String {
        guard let p = copad_muxhost_copad_tag(panel, guiSocket) else { return "" }
        defer { copad_ffi_free_string(p) }
        return String(cString: p)
    }

    /// A window now shows `panels` (every terminal split of its visible tab).
    func serve(window: ObjectIdentifier, panels: [String]) {
        guard let h = handle else { return }
        for p in panels { copad_muxhost_bind(h, p, tag(for: p)) }
        served[window] = panels
        let all = served.values.flatMap { $0 }
        if let data = try? JSONEncoder().encode(all), let s = String(data: data, encoding: .utf8) {
            copad_muxhost_serve(h, s)
        }
    }

    func windowClosed(_ window: ObjectIdentifier) {
        served[window] = nil
        serve(window: window, panels: [])
        served[window] = nil
    }

    func panelClosed(_ panel: String) {
        guard let h = handle else { return }
        copad_muxhost_unbind(h, panel)
    }

    /// Whether this window should draw the native sidebar: one of its shown panels runs a
    /// comux client whose sidebar we hold.
    func hosts(panels: [String]) -> Bool {
        guard let s = state else { return false }
        return panels.contains { p in s.bound[p].map { s.claimed.contains($0) } ?? false }
    }

    func command(_ req: [String: Any]) {
        guard let h = handle, let d = try? JSONSerialization.data(withJSONObject: req),
              let s = String(data: d, encoding: .utf8) else { return }
        copad_muxhost_command(h, s)
    }
}

/// The left panel: spaces, then every agent with its status and time in it.
struct ComuxSidebar: View {
    @ObservedObject var host: ComuxHost
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
    @ObservedObject var host: ComuxHost
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
