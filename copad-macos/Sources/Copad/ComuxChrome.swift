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
        /// Pane previews by consumer (a window), decisions #180.
        var previews: [String: L0PreviewState]?
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
        for link in links.values { copad_muxhost_preview(link.handle, Self.consumer(window), nil, 0, 0) }
    }

    /// A window's name for its pane preview.
    nonisolated static func consumer(_ window: ObjectIdentifier) -> String { "w\(UInt(bitPattern: window.hashValue))" }

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

    /// Ask comux to open one of its overlays for `panel`'s client (decisions #172): it comes
    /// back as a delegated overlay, so a rename gets comux's seeded prompt.
    func open(key: String, panel: String, _ what: String, target: String?) {
        guard let link = links[key] else { return }
        copad_muxhost_open(link.handle, panel, what, target)
    }

    /// Answer with a staying choice (the overlay stays until comux re-sends it).
    func act(key: String, _ overlay: UInt64, choice: String) {
        guard let link = links[key] else { return }
        copad_muxhost_act(link.handle, overlay, choice)
    }

    /// Keep `consumer`'s preview of `pane` fresh; nil stops it.
    func preview(key: String, consumer: String, pane: String?, rows: Int, cols: Int) {
        guard let link = links[key] else { return }
        copad_muxhost_preview(link.handle, consumer, pane, UInt16(clamping: rows), UInt16(clamping: cols))
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
    /// The window's terminal font and theme, so the chrome reads like the TUI it replaces.
    @Published var style = ComuxStyle.make(config: .defaults, theme: .default)
    /// The notification panel floats over the terminal (a narrow window) instead of docking.
    @Published var notificationsFloating = false
    private(set) var key: String?
    /// This window's name for its pane preview (decisions #180).
    var consumer = ""

    /// The visible panel whose comux client this window hosts — what host-initiated overlays
    /// are opened for.
    private(set) var panel: String?

    func update(panels: [String]) {
        let host = ComuxHost.shared
        key = host.key(for: panels)
        state = key.flatMap { host.states[$0] }
        panel = state.flatMap { s in panels.first { p in s.bound[p].map { s.claimed.contains($0) } ?? false } }
    }

    /// `new-session`, `resume`, `rename-session`, `kill-session`, `rename-tab`, `close-tab`, …
    func open(_ what: String, target: String? = nil) {
        if let key, let panel { ComuxHost.shared.open(key: key, panel: panel, what, target: target) }
    }

    func command(_ req: [String: Any]) {
        if let key { ComuxHost.shared.command(key: key, req) }
    }

    func answer(_ overlay: UInt64, choice: String?, text: String?) {
        if let key { ComuxHost.shared.answer(key: key, overlay, choice: choice, text: text) }
    }

    func act(_ overlay: UInt64, choice: String) {
        if let key { ComuxHost.shared.act(key: key, overlay, choice: choice) }
    }

    func preview(pane: String?, rows: Int, cols: Int) {
        if let key { ComuxHost.shared.preview(key: key, consumer: consumer, pane: pane, rows: rows, cols: cols) }
    }

    var previewState: L0PreviewState? { state?.previews?[consumer] }
}

/// A delegated overlay over the whole window content (decisions #180): it reads the window's
/// live state, so a re-sent spec (rows landed, an agent changed) and the preview update in place.
struct ComuxOverlayHost: View {
    @ObservedObject var host: ComuxWindow
    let id: UInt64
    /// The overlay is gone here (answered or cancelled).
    let closed: () -> Void

    var body: some View {
        if let o = host.state?.overlays.first(where: { $0.overlay == id }) {
            ComuxOverlayLayer(
                overlay: o, look: host.style.overlayLook,
                context: OverlayContext(agents: host.state?.agents?.agents ?? [],
                                        sessions: host.state?.tree?.sessions ?? [],
                                        preview: host.previewState),
                handlers: OverlayHandlers(
                    answer: { choice, text in
                        host.answer(id, choice: choice, text: text)
                        closed()
                    },
                    act: { host.act(id, choice: $0) },
                    preview: { pane, rows, cols in host.preview(pane: pane, rows: rows, cols: cols) }))
        }
    }
}

// MARK: - Look (decisions #169, polished in #178)

/// The chrome's typography and colours: the terminal's font and the copad theme, mapped onto
/// the roles the comux TUI paints its sidebar and status bar with, so the native chrome reads
/// as the same thing and not as a stock AppKit panel.
struct ComuxStyle: Equatable {
    var font: NSFont
    var bold: NSFont
    var lineHeight: CGFloat
    var advance: CGFloat
    var base: Color
    var text: Color
    var subtext: Color
    var dim: Color
    var rule: Color
    var hover: Color
    var accent: Color
    var mauve: Color
    var peach: Color
    var green: Color
    var yellow: Color
    var blue: Color
    var red: Color
    /// The terminal's default foreground and ANSI colours (overlay previews).
    var fg: Color
    var ansi: [Color]

    @MainActor
    static func make(config: CopadConfig, theme: CopadTheme) -> ComuxStyle {
        let size = CGFloat(max(config.fontSize, 9))
        let font = resolveTerminalFont(family: config.fontFamily, size: size)
        let bold = NSFontManager.shared.convert(font, toHaveTrait: .boldFontMask)
        let line = ceil(font.ascender - font.descender + font.leading)
        let advance = ("M" as NSString).size(withAttributes: [.font: font]).width
        let p = theme.palette
        func c(_ x: RGBColor) -> Color { Color(nsColor: x.nsColor) }
        func ansi(_ i: Int, _ fallback: RGBColor) -> Color { c(p.indices.contains(i) ? p[i] : fallback) }
        func mix(_ a: Int, _ b: Int) -> Color {
            guard p.indices.contains(a), p.indices.contains(b) else { return c(theme.accent) }
            return Color(red: (Double(p[a].r) + Double(p[b].r)) / 510, green: (Double(p[a].g) + Double(p[b].g)) / 510,
                         blue: (Double(p[a].b) + Double(p[b].b)) / 510)
        }
        return ComuxStyle(
            font: font, bold: bold, lineHeight: line, advance: max(advance, 1),
            base: c(theme.background), text: c(theme.text), subtext: c(theme.subtext1), dim: c(theme.subtext0),
            rule: c(theme.surface2), hover: c(theme.surface1), accent: c(theme.accent),
            // The TUI's mauve (active session) and peach (a session running agents) are not
            // ANSI colours: mix them from the theme's own so a light theme stays legible.
            mauve: mix(5, 4), peach: mix(1, 3),
            green: ansi(2, theme.accent), yellow: ansi(3, theme.accent), blue: ansi(4, theme.accent),
            red: c(theme.red),
            fg: c(theme.text), ansi: p.map(c)
        )
    }

    var overlayLook: OverlayLook {
        OverlayLook(body: body, strong: strong, lineHeight: lineHeight, advance: advance, base: base, text: text,
                    subtext: subtext, dim: dim, rule: rule, hover: hover, accent: accent, mauve: mauve, peach: peach,
                    green: green, yellow: yellow, blue: blue, red: red, fg: fg, ansi: ansi)
    }

    var body: Font { Font(font) }
    var strong: Font { Font(bold) }
    /// A two-line row (name + subtitle), with its padding.
    var entryHeight: CGFloat { lineHeight * 2 + 6 }
    var oneLineHeight: CGFloat { lineHeight + 4 }
    var statusHeight: CGFloat { ceil(lineHeight) + 8 }

    func status(_ s: String) -> (glyph: String, color: Color) {
        switch s {
        case "working": return ("●", green)
        case "ready": return ("◐", blue)
        case "blocked": return ("▲", yellow)
        default: return ("○", dim)
        }
    }

    /// green < 70% ≤ yellow < 90% ≤ red, like the TUI's gauges.
    func usage(_ pct: Double) -> Color { pct >= 90 ? red : pct >= 70 ? yellow : green }
}

/// How the sidebar's height is shared (the TUI's `split_sidebar`): a half that needs less than
/// its share gives the rest to the other; when both overflow they split evenly. Each keeps at
/// least its header and one entry.
func comuxSplit(total: CGFloat, spaces: CGFloat, agents: CGFloat, floor: CGFloat) -> CGFloat {
    let half = total / 2
    let top: CGFloat
    if spaces + agents <= total || spaces <= half {
        top = spaces
    } else if agents <= half {
        top = total - agents
    } else {
        top = half
    }
    return min(max(top, floor), max(total - floor, floor))
}

/// A row that highlights under the pointer, like a TUI selection but quieter.
private struct HoverRow: ViewModifier {
    var style: ComuxStyle
    @State private var over = false
    func body(content: Content) -> some View {
        content
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: 4).fill(over ? style.hover.opacity(0.8) : .clear))
            .onHover { over = $0 }
    }
}

private extension View {
    func comuxHover(_ s: ComuxStyle) -> some View { modifier(HoverRow(style: s)) }
}

/// `  spaces ───` / `  agents ⚑2 ───`: a section header in the TUI's dim bold.
private struct SectionHeader: View {
    var style: ComuxStyle
    var title: String
    var badge: String?
    var body: some View {
        HStack(spacing: 6) {
            Text(title).font(style.strong).foregroundStyle(style.dim)
            if let badge { Text(badge).font(style.strong).foregroundStyle(style.yellow) }
            Rectangle().fill(style.rule).frame(height: 1)
        }
        .padding(.horizontal, 8)
        .frame(height: style.oneLineHeight)
    }
}

/// The left panel: spaces on top, agents below, each scrolling on its own (decisions #178). It
/// is transparent — the window's wallpaper runs under it — with a one-pixel rule on its right,
/// the TUI's `│`.
struct ComuxSidebar: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void
    var toggleNotifications: () -> Void

    var body: some View {
        let st = host.style
        let sessions = host.state?.tree?.sessions ?? []
        let agents = host.state?.agents?.agents ?? []
        let blocked = agents.filter { $0.status == "blocked" }
        let groups = agentGroups(agents)
        let grouped = groups.count > 1
        VStack(spacing: 0) {
            toolbar(st)
            GeometryReader { geo in
                let spacesNeed = st.oneLineHeight + CGFloat(sessions.count) * st.entryHeight
                let agentsNeed = st.oneLineHeight
                    + CGFloat(blocked.count) * st.oneLineHeight
                    + (grouped ? CGFloat(groups.count) * st.oneLineHeight : 0)
                    + CGFloat(agents.count) * st.entryHeight
                let top = comuxSplit(total: geo.size.height, spaces: spacesNeed, agents: agentsNeed,
                                     floor: st.oneLineHeight + st.entryHeight)
                VStack(spacing: 0) {
                    VStack(spacing: 0) {
                        SectionHeader(style: st, title: "spaces")
                        ScrollView(.vertical) {
                            LazyVStack(spacing: 0) {
                                ForEach(sessions) { space($0, st) }
                            }
                        }
                        .scrollIndicators(.automatic)
                    }
                    .frame(height: top)
                    VStack(spacing: 0) {
                        SectionHeader(style: st, title: "agents", badge: blocked.isEmpty ? nil : "⚑\(blocked.count)")
                        ScrollView(.vertical) {
                            LazyVStack(spacing: 0) {
                                // Waiting agents, pinned: duplicated rather than sorted to the
                                // top, so the list below never moves under the pointer.
                                ForEach(blocked) { band($0, st) }
                                if !blocked.isEmpty { Rectangle().fill(st.rule).frame(height: 1).padding(.horizontal, 8) }
                                ForEach(groups, id: \.id) { g in
                                    if grouped { groupHeader(g, st) }
                                    ForEach(g.agents) { agent($0, grouped: grouped, st) }
                                }
                            }
                        }
                        .scrollIndicators(.automatic)
                    }
                    .frame(maxHeight: .infinity)
                }
            }
        }
        .overlay(alignment: .trailing) { Rectangle().fill(st.rule).frame(width: 1) }
        .accessibilityIdentifier("comuxSidebar")
    }

    private func toolbar(_ st: ComuxStyle) -> some View {
        HStack(spacing: 10) {
            Text("comux").font(st.strong).foregroundStyle(st.mauve)
            Spacer()
            Button { host.open("new-session") } label: { Image(systemName: "plus") }
                .help("New session… (Ctrl-b C)")
                .accessibilityIdentifier("comuxNewSession")
            Button { host.open("resume") } label: { Image(systemName: "clock.arrow.circlepath") }
                .help("Resume a conversation… (Ctrl-b R)")
                .accessibilityIdentifier("comuxResume")
            Button(action: toggleNotifications) {
                HStack(spacing: 2) {
                    Image(systemName: "bell")
                    if let n = host.state?.notifications?.unread, n > 0 {
                        Text("\(n)").font(st.strong).foregroundStyle(st.yellow)
                    }
                }
            }
            .help("Notifications (Ctrl-b a)")
            .accessibilityIdentifier("comuxNotificationsToggle")
        }
        .buttonStyle(.borderless)
        .foregroundStyle(st.subtext)
        .padding(.horizontal, 8)
        .frame(height: st.oneLineHeight + 6)
    }

    private func space(_ s: L0Session, _ st: ComuxStyle) -> some View {
        let agents = s.tabs.contains { $0.panes.contains { $0.kind == "agent" } }
        let bell = s.tabs.contains { $0.bell }
        return Button {
            settle()
            host.command(["cmd": "select-session", "target": s.id])
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text("●").foregroundStyle(s.active ? st.mauve : agents ? st.peach : st.dim)
                    Text(s.name).font(s.active ? st.strong : st.body)
                        .foregroundStyle(s.active ? st.text : st.subtext).lineLimit(1)
                    if bell { Text("!").font(st.strong).foregroundStyle(st.yellow) }
                    Spacer(minLength: 0)
                }
                Text(s.subtitle).foregroundStyle(st.dim).lineLimit(1)
                    .padding(.leading, st.advance * 2)
            }
            .font(st.body)
            .padding(.horizontal, 8)
            .frame(height: st.entryHeight)
            .comuxHover(st)
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button("Rename Session…") { host.open("rename-session", target: s.id) }
            Button("New Session…") { host.open("new-session") }
            Divider()
            Button("Kill Session…", role: .destructive) { host.open("kill-session", target: s.id) }
        }
        .accessibilityIdentifier("comux-space-\(s.name)")
    }

    private func jump(_ a: L0Agent) {
        settle()
        host.command(["cmd": "jump", "target": a.token])
    }

    private func band(_ a: L0Agent, _ st: ComuxStyle) -> some View {
        Button { jump(a) } label: {
            HStack(spacing: 6) {
                Text("▲").foregroundStyle(st.yellow)
                Text("\(a.space)/\(a.title)").foregroundStyle(st.text).lineLimit(1)
                Spacer(minLength: 4)
                Text(elapsedLabel(since: a.since_unix)).foregroundStyle(st.yellow)
            }
            .font(st.body)
            .padding(.horizontal, 8)
            .frame(height: st.oneLineHeight)
            .comuxHover(st)
        }
        .buttonStyle(.plain)
    }

    private func groupHeader(_ g: AgentGroup, _ st: ComuxStyle) -> some View {
        Button {
            settle()
            host.command(["cmd": "select-session", "target": g.id])
        } label: {
            HStack(spacing: 6) {
                Text(g.name).font(st.body).foregroundStyle(st.dim).lineLimit(1)
                Rectangle().fill(st.rule).frame(height: 1)
            }
            .padding(.horizontal, 8)
            .frame(height: st.oneLineHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func agent(_ a: L0Agent, grouped: Bool, _ st: ComuxStyle) -> some View {
        let (glyph, color) = st.status(a.status)
        return Button { jump(a) } label: {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(glyph).foregroundStyle(color)
                    Text(grouped ? a.title : "\(a.space)/\(a.title)")
                        .font(a.focused ? st.strong : st.body)
                        .foregroundStyle(a.focused ? st.text : st.subtext).lineLimit(1)
                    Spacer(minLength: 4)
                    Text("\(a.status) \(elapsedLabel(since: a.since_unix))").foregroundStyle(color).lineLimit(1)
                }
                Text(a.detail ?? a.tool).foregroundStyle(st.dim).lineLimit(1)
                    .padding(.leading, st.advance * 2)
            }
            .font(st.body)
            .padding(.horizontal, 8)
            .frame(height: st.entryHeight)
            .comuxHover(st)
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button("Jump to Pane") { jump(a) }
            Button("Rename Tab…") { host.open("rename-tab", target: "\(a.space_id)/\(a.tab_id)") }
        }
    }

    struct AgentGroup {
        var id: String
        var name: String
        var agents: [L0Agent]
    }

    /// Agents under their space, in the order comux lists them.
    private func agentGroups(_ agents: [L0Agent]) -> [AgentGroup] {
        var out: [AgentGroup] = []
        for a in agents {
            if let i = out.firstIndex(where: { $0.id == a.space_id }) {
                out[i].agents.append(a)
            } else {
                out.append(AgentGroup(id: a.space_id, name: a.space, agents: [a]))
            }
        }
        return out
    }
}

/// The right panel: the notification center, newest first. A click opens the notification —
/// its pane comes up and that pane's notifications leave the list; ✕ dismisses one.
struct ComuxNotifications: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void

    var body: some View {
        let st = host.style
        let items = host.state?.notifications?.items ?? []
        VStack(spacing: 0) {
            HStack {
                Text("notifications").font(st.strong).foregroundStyle(st.dim)
                Spacer()
                Button("Clear all") { host.command(["cmd": "notif-dismiss"]) }
                    .buttonStyle(.borderless).font(st.body).foregroundStyle(st.subtext)
                    .disabled(items.isEmpty)
            }
            .padding(.horizontal, 10)
            .frame(height: st.oneLineHeight + 6)
            Rectangle().fill(st.rule).frame(height: 1)
            if items.isEmpty {
                Text("no notifications").font(st.body).foregroundStyle(st.dim).padding(.top, 12)
                Spacer()
            } else {
                ScrollView(.vertical) {
                    LazyVStack(spacing: 0) {
                        ForEach(items) { row($0, st) }
                    }
                }
            }
        }
        .background(host.notificationsFloating ? st.base.opacity(0.97) : .clear)
        .overlay(alignment: .leading) { Rectangle().fill(st.rule).frame(width: 1) }
        .shadow(color: .black.opacity(host.notificationsFloating ? 0.35 : 0), radius: 12, x: -4)
        .accessibilityIdentifier("comuxNotifications")
    }

    private func open(_ n: L0Notification) {
        settle()
        host.command(["cmd": "notif-read", "id": n.id])
        if n.token.isEmpty {
            host.command(["cmd": "notif-dismiss", "id": n.id])
        } else {
            // `consume` removes the pane's notifications as the view moves there; a comux
            // older than that ignores it and only jumps.
            host.command(["cmd": "jump", "target": n.token, "consume": true])
        }
    }

    private func row(_ n: L0Notification, _ st: ComuxStyle) -> some View {
        let blocked = n.kind == "blocked"
        return HStack(alignment: .top, spacing: 6) {
            Button { open(n) } label: {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(blocked ? "▲" : "✓").foregroundStyle(blocked ? st.yellow : st.green)
                        Text("\(n.tool) · \(n.space)/\(n.tab)")
                            .font(n.read ? st.body : st.strong)
                            .foregroundStyle(n.read ? st.subtext : st.text).lineLimit(1)
                        Spacer(minLength: 4)
                        Text(n.when).foregroundStyle(st.dim)
                    }
                    Text(n.body).foregroundStyle(st.subtext).lineLimit(2)
                        .padding(.leading, st.advance * 2)
                }
                .font(st.body)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("comux-notif-\(n.id)")
            Button { host.command(["cmd": "notif-dismiss", "id": n.id]) } label: {
                Image(systemName: "xmark").foregroundStyle(st.dim)
            }
            .buttonStyle(.borderless)
            .help("Dismiss")
            .accessibilityIdentifier("comux-notif-dismiss-\(n.id)")
        }
        .padding(.horizontal, 10).padding(.vertical, 6)
        .comuxHover(st)
    }
}

/// comux's status bar as a native strip (decisions #170, #178): session pill, tab chips, the
/// armed prefix, the outcome line, counters, usage gauges, update, clock and host — laid out
/// like the TUI's bar. The tab chips scroll when they do not fit; the right cluster keeps its
/// room and sheds detail (reset times, then gauges, then all but one window) instead.
struct ComuxStatusStrip: View {
    @ObservedObject var host: ComuxWindow
    var settle: () -> Void

    var body: some View {
        let st = host.style
        let s = host.state
        let status = s?.status
        let session = s?.tree?.sessions.first { $0.active }
        HStack(spacing: 8) {
            if let session { pill(session, st) }
            ScrollViewReader { proxy in
                ScrollView(.horizontal) {
                    HStack(spacing: 4) {
                        ForEach(session?.tabs ?? []) { chip($0, session: session, st).id($0.id) }
                    }
                }
                .scrollIndicators(.never)
                .onChange(of: session?.tabs.first { $0.active }?.id) { _, id in
                    if let id { withAnimation { proxy.scrollTo(id) } }
                }
            }
            if status?.prefix_armed == true {
                badge(status?.prefix_label ?? "^b", fg: st.base, bg: st.yellow, st)
            }
            if let f = status?.flash {
                Text(f.text).font(st.body).foregroundStyle(f.error ? st.red : st.subtext).lineLimit(1)
            }
            Spacer(minLength: 8)
            HStack(spacing: 6) {
                if let b = status?.bells, b > 0 { badge("! \(b)", fg: st.base, bg: st.red, st) }
                if let a = status?.attention, a > 0 { badge("⚑\(a)", fg: st.base, bg: st.yellow, st) }
                if let n = status?.agents, n > 0 { Text("● \(n)").foregroundStyle(st.peach) }
                if let u = status?.unread, u > 0 { Text("✉ \(u)").foregroundStyle(st.blue) }
                if let windows = status?.usage?.windows, !windows.isEmpty { ComuxUsage(windows: windows, style: st) }
                if let up = status?.update { Text("⬆ \(up)").foregroundStyle(st.green) }
                TimelineView(.everyMinute) { ctx in
                    Text(ctx.date, style: .time).foregroundStyle(st.subtext)
                }
                if let h = status?.host, !h.isEmpty { Text(h).font(st.strong).foregroundStyle(st.text) }
            }
            .font(st.body)
            .fixedSize(horizontal: false, vertical: true)
            .layoutPriority(1)
        }
        .font(st.body)
        .padding(.horizontal, 8)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(st.base.opacity(0.92))
        .overlay(alignment: .top) { Rectangle().fill(st.rule).frame(height: 1) }
        .accessibilityIdentifier("comuxStatus")
    }

    private func badge(_ text: String, fg: Color, bg: Color, _ st: ComuxStyle) -> some View {
        Text(text).font(st.strong).foregroundStyle(fg)
            .padding(.horizontal, 6).padding(.vertical, 1)
            .background(RoundedRectangle(cornerRadius: 4).fill(bg))
    }

    private func pill(_ s: L0Session, _ st: ComuxStyle) -> some View {
        badge(s.name, fg: st.base, bg: st.mauve, st)
            .contextMenu {
                Button("Rename Session…") { host.open("rename-session", target: s.id) }
                Button("New Session…") { host.open("new-session") }
                Divider()
                Button("Kill Session…", role: .destructive) { host.open("kill-session", target: s.id) }
            }
    }

    private func chip(_ t: L0Tab, session: L0Session?, _ st: ComuxStyle) -> some View {
        let agent = t.panes.contains { $0.kind == "agent" }
        return Button {
            settle()
            host.command(["cmd": "select-tab", "target": t.id])
        } label: {
            HStack(spacing: 3) {
                if agent { Text("●").foregroundStyle(t.active ? st.base : st.yellow) }
                Text("\(t.index + 1) \(t.title)").lineLimit(1)
                if t.bell { Text("!").font(st.strong).foregroundStyle(t.active ? st.base : st.red) }
            }
            .font(t.active ? st.strong : st.body)
            .foregroundStyle(t.active ? st.base : agent ? st.yellow : st.subtext)
            .padding(.horizontal, 7).padding(.vertical, 1)
            .background(RoundedRectangle(cornerRadius: 4).fill(t.active ? st.blue : st.rule.opacity(0.7)))
        }
        .buttonStyle(.plain)
        .contextMenu {
            if let sid = session?.id {
                Button("Rename Tab…") { host.open("rename-tab", target: "\(sid)/\(t.id)") }
                Button("New Tab") { settle(); host.command(["cmd": "new-tab"]) }
                Divider()
                Button("Close Tab…", role: .destructive) { host.open("close-tab", target: "\(sid)/\(t.id)") }
            }
        }
    }
}

/// Subscription usage like the TUI's readout: per window a threshold-coloured gauge, the
/// percentage and the time to reset (`⟳ 2h13m`, the exact time in the tooltip). When the bar
/// is short of room it drops the reset times, then the gauges, then shows one window at a time
/// (click for the next).
struct ComuxUsage: View {
    var windows: [L0UsageWindow]
    var style: ComuxStyle
    @State private var page = 0

    var body: some View {
        ViewThatFits(in: .horizontal) {
            row(gauge: true, reset: true)
            row(gauge: true, reset: false)
            row(gauge: false, reset: false)
            paged
        }
    }

    private func row(gauge: Bool, reset: Bool) -> some View {
        HStack(spacing: 10) {
            ForEach(Array(providers.enumerated()), id: \.offset) { _, p in
                HStack(spacing: 6) {
                    Text(p.name).foregroundStyle(style.dim)
                    ForEach(Array(p.windows.enumerated()), id: \.offset) { _, w in
                        item(w, label: w.window, gauge: gauge, reset: reset)
                    }
                }
            }
        }
        .fixedSize()
    }

    private var paged: some View {
        let w = windows[page % windows.count]
        return Button { page = (page + 1) % windows.count } label: {
            HStack(spacing: 4) {
                item(w, label: "\(w.provider) \(w.window)", gauge: false, reset: false)
                if windows.count > 1 { Text("\(page % windows.count + 1)/\(windows.count)").foregroundStyle(style.dim) }
            }
        }
        .buttonStyle(.plain)
        .fixedSize()
        .help("Click for the next window")
    }

    private func item(_ w: L0UsageWindow, label: String, gauge: Bool, reset: Bool) -> some View {
        let color = style.usage(w.pct)
        return HStack(spacing: 4) {
            Text(label).foregroundStyle(style.subtext)
            if gauge {
                ZStack(alignment: .leading) {
                    Capsule().fill(style.rule)
                    Capsule().fill(color).frame(width: 56 * min(max(w.pct, 0), 100) / 100)
                }
                .frame(width: 56, height: max(4, style.lineHeight * 0.32))
            }
            Text("\(w.stale ? "~" : "")\(Int(w.pct.rounded()))%").foregroundStyle(color)
            if reset, let at = w.resets_at {
                Text("⟳ \(untilLabel(at))").foregroundStyle(style.dim)
            }
        }
        .help(w.resets_at.map { "\(w.provider) \(w.window) resets \(Date(timeIntervalSince1970: TimeInterval($0)).formatted(date: .abbreviated, time: .shortened))" } ?? "\(w.provider) \(w.window)")
    }

    private struct Provider {
        var name: String
        var windows: [L0UsageWindow]
    }

    private var providers: [Provider] {
        var out: [Provider] = []
        for w in windows {
            if let i = out.firstIndex(where: { $0.name == w.provider }) {
                out[i].windows.append(w)
            } else {
                out.append(Provider(name: w.provider, windows: [w]))
            }
        }
        return out
    }
}

/// `2h13m` / `5d19h` until `at` (unix seconds), as the TUI's relative reset.
func untilLabel(_ at: Int64, now: Date = Date()) -> String {
    let secs = max(0, at - Int64(now.timeIntervalSince1970))
    let m = secs / 60
    if m < 60 { return "\(m)m" }
    let h = m / 60
    if h < 24 { return "\(h)h\(m % 60)m" }
    return "\(h / 24)d\(h % 24)h"
}
