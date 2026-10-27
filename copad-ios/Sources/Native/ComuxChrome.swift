import SwiftUI

/// comux's chrome on the phone, drawn like the comux TUI it replaces (decisions #179): the
/// terminal's bundled Nerd Font, the TUI's own Catppuccin colours (the app has no theme
/// setting; these are comux's `CAT_*` constants), spaces and agents scrolling separately,
/// usage as gauges.
enum ComuxLook {
    static let size: CGFloat = 13
    static let body = Font.custom("JetBrainsMonoNFM-Regular", size: size)
    static let strong = Font.custom("JetBrainsMonoNFM-Bold", size: size)
    static let lineHeight: CGFloat = 17
    /// A two-line entry, tall enough to tap.
    static let entryHeight: CGFloat = 46
    static let oneLineHeight: CGFloat = 30

    static let base = Color(hex: 0x1E1E2E)
    static let text = Color(hex: 0xCDD6F4)
    static let subtext = Color(hex: 0xBAC2DE)
    static let dim = Color(hex: 0x6C7086)
    static let rule = Color(hex: 0x313244)
    static let hover = Color(hex: 0x45475A)
    static let mauve = Color(hex: 0xCBA6F7)
    static let peach = Color(hex: 0xFAB387)
    static let green = Color(hex: 0xA6E3A1)
    static let yellow = Color(hex: 0xF9E2AF)
    static let blue = Color(hex: 0x89B4FA)
    static let red = Color(hex: 0xF38BA8)

    static func status(_ s: String) -> (glyph: String, color: Color) {
        switch s {
        case "working": return ("●", green)
        case "ready": return ("◐", blue)
        case "blocked": return ("▲", yellow)
        default: return ("○", dim)
        }
    }

    /// green < 70% ≤ yellow < 90% ≤ red, like the TUI's gauges.
    static func usage(_ pct: Double) -> Color { pct >= 90 ? red : pct >= 70 ? yellow : green }

    /// The overlays' look (decisions #180): the same font and colours, the TUI's ANSI palette.
    static let overlay = OverlayLook(
        body: body, strong: strong, lineHeight: lineHeight, advance: size * 0.6, base: base, text: text,
        subtext: subtext, dim: dim, rule: rule, hover: hover, accent: blue, mauve: mauve, peach: peach,
        green: green, yellow: yellow, blue: blue, red: red, fg: text,
        ansi: Palette.ansi.map { Color(red: $0.r, green: $0.g, blue: $0.b) })
}

/// A drawer row's pane, shown on long-press above its menu (decisions #181).
struct DrawerPreview: View {
    @ObservedObject var session: MuxSession
    let pane: String?

    var body: some View {
        PanePreview(pane: pane, look: ComuxLook.overlay, state: session.previews["drawer"],
                    handlers: OverlayHandlers(answer: { _, _ in }, act: { _ in },
                                              preview: { p, r, c in session.setPreview(owner: "drawer", p, rows: r, cols: c) }))
            .frame(width: 340, height: 280)
            .background(ComuxLook.base)
            .onDisappear { session.setPreview(owner: "drawer", nil, rows: 0, cols: 0) }
    }
}

extension Color {
    init(hex: UInt32) {
        self.init(red: Double(hex >> 16 & 0xFF) / 255, green: Double(hex >> 8 & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255)
    }
}

/// How the drawer's height is shared (the TUI's `split_sidebar`): a half that needs less than
/// its share gives the rest to the other; both overflowing split evenly. When even the floors do
/// not fit, spaces yields first, down to its header.
func comuxSplit(total: CGFloat, spaces: CGFloat, agents: CGFloat, floor: CGFloat, header: CGFloat) -> CGFloat {
    let half = total / 2
    let top: CGFloat
    if spaces + agents <= total || spaces <= half {
        top = spaces
    } else if agents <= half {
        top = total - agents
    } else {
        top = half
    }
    return max(min(max(top, floor), max(total - floor, header)), min(header, total))
}

/// `2h13m` / `5d19h` until `at` (unix seconds).
func untilLabel(_ at: Int64, now: Date = Date()) -> String {
    let m = max(0, at - Int64(now.timeIntervalSince1970)) / 60
    if m < 60 { return "\(m)m" }
    let h = m / 60
    if h < 24 { return "\(h)h\(m % 60)m" }
    return "\(h / 24)d\(h % 24)h"
}

/// `  spaces ───` / `  agents ⚑2 ───`.
private struct SectionHeader: View {
    var title: String
    var badge: String? = nil
    var body: some View {
        HStack(spacing: 6) {
            Text(title).font(ComuxLook.strong).foregroundStyle(ComuxLook.dim)
            if let badge { Text(badge).font(ComuxLook.strong).foregroundStyle(ComuxLook.yellow) }
            Rectangle().fill(ComuxLook.rule).frame(height: 1)
        }
        .padding(.horizontal, 12)
        .frame(height: ComuxLook.oneLineHeight)
    }
}

/// The left drawer: spaces on top, agents below, each scrolling on its own.
struct SpacesDrawer: View {
    @ObservedObject var session: MuxSession
    var act: (() -> Void) -> Void

    var body: some View {
        let sessions = session.model.tree?.sessions ?? []
        let agents = session.model.agents?.agents ?? []
        let blocked = agents.filter { $0.status == "blocked" }
        let groups = agentGroups(agents)
        let grouped = groups.count > 1
        // Ages and "idle 4m" move on while nothing else changes.
        TimelineView(.periodic(from: .now, by: 30)) { _ in
            VStack(spacing: 0) {
                toolbar
                GeometryReader { geo in
                    let L = ComuxLook.self
                    let spacesNeed = L.oneLineHeight + CGFloat(sessions.count) * L.entryHeight
                    let agentsNeed = L.oneLineHeight + CGFloat(blocked.count) * L.oneLineHeight
                        + (grouped ? CGFloat(groups.count) * L.oneLineHeight : 0)
                        + CGFloat(max(agents.count, 1)) * L.entryHeight
                    let top = comuxSplit(total: geo.size.height, spaces: spacesNeed, agents: agentsNeed,
                                         floor: L.oneLineHeight + L.entryHeight, header: L.oneLineHeight)
                    VStack(spacing: 0) {
                        VStack(spacing: 0) {
                            SectionHeader(title: "spaces")
                            ScrollView {
                                LazyVStack(spacing: 0) { ForEach(sessions) { space($0) } }
                            }
                        }
                        .frame(height: top)
                        VStack(spacing: 0) {
                            SectionHeader(title: "agents", badge: blocked.isEmpty ? nil : "⚑\(blocked.count)")
                            ScrollView {
                                LazyVStack(spacing: 0) {
                                    if agents.isEmpty {
                                        Text("no agents").font(ComuxLook.body).foregroundStyle(ComuxLook.dim)
                                            .frame(maxWidth: .infinity, alignment: .leading).padding(12)
                                    }
                                    ForEach(blocked) { band($0) }
                                    if !blocked.isEmpty {
                                        Rectangle().fill(ComuxLook.rule).frame(height: 1).padding(.horizontal, 12)
                                    }
                                    ForEach(groups, id: \.id) { g in
                                        if grouped { groupHeader(g) }
                                        ForEach(g.agents) { agent($0, grouped: grouped) }
                                    }
                                }
                            }
                        }
                        .frame(maxHeight: .infinity)
                    }
                }
            }
        }
        .frame(width: 300)
        .background(ComuxLook.base)
        .overlay(alignment: .trailing) { Rectangle().fill(ComuxLook.rule).frame(width: 1) }
        .ignoresSafeArea(.keyboard)
        .accessibilityIdentifier("spacesDrawer")
    }

    private var toolbar: some View {
        HStack(spacing: 16) {
            Text("comux").font(ComuxLook.strong).foregroundStyle(ComuxLook.mauve)
            Spacer()
            Button { act { session.openOverlay("new-session") } } label: { Image(systemName: "plus") }
                .accessibilityIdentifier("drawerNewSession")
            Button { act { session.openOverlay("resume") } } label: { Image(systemName: "clock.arrow.circlepath") }
                .accessibilityIdentifier("drawerResume")
        }
        .foregroundStyle(ComuxLook.subtext)
        .padding(.horizontal, 12)
        .frame(height: 40)
    }

    private func space(_ s: L0Session) -> some View {
        let agents = s.tabs.contains { $0.panes.contains { $0.kind == "agent" } }
        let bell = s.tabs.contains { $0.bell }
        return Button { act { session.selectSession(s.id) } } label: {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text("●").foregroundStyle(s.active ? ComuxLook.mauve : agents ? ComuxLook.peach : ComuxLook.dim)
                    Text(s.name).font(s.active ? ComuxLook.strong : ComuxLook.body)
                        .foregroundStyle(s.active ? ComuxLook.text : ComuxLook.subtext).lineLimit(1)
                    if bell { Text("!").font(ComuxLook.strong).foregroundStyle(ComuxLook.yellow) }
                    Spacer(minLength: 0)
                }
                Text(s.subtitle).foregroundStyle(ComuxLook.dim).lineLimit(1).padding(.leading, 16)
            }
            .font(ComuxLook.body)
            .padding(.horizontal, 12)
            .frame(height: ComuxLook.entryHeight)
            .background(s.active ? ComuxLook.rule.opacity(0.5) : .clear)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button("Rename session…") { act { session.openOverlay("rename-session", target: s.id) } }
            Button("New session…") { act { session.openOverlay("new-session") } }
            Button("Kill session…", role: .destructive) { act { session.openOverlay("kill-session", target: s.id) } }
        } preview: {
            DrawerPreview(session: session, pane: s.tabs.first { $0.active }?.panes.first { $0.focused }?.token)
        }
        .accessibilityIdentifier("space-\(s.name)")
    }

    private func band(_ a: L0Agent) -> some View {
        Button { act { session.jump(a.token) } } label: {
            HStack(spacing: 6) {
                Text("▲").foregroundStyle(ComuxLook.yellow)
                Text("\(a.space)/\(a.title)").foregroundStyle(ComuxLook.text).lineLimit(1)
                Spacer(minLength: 4)
                Text(elapsedLabel(since: a.since_unix)).foregroundStyle(ComuxLook.yellow)
            }
            .font(ComuxLook.body)
            .padding(.horizontal, 12)
            .frame(height: ComuxLook.oneLineHeight + 8)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("band-\(a.token)")
    }

    private func groupHeader(_ g: AgentGroup) -> some View {
        Button { act { session.selectSession(g.id) } } label: {
            HStack(spacing: 6) {
                Text(g.name).font(ComuxLook.body).foregroundStyle(ComuxLook.dim).lineLimit(1)
                Rectangle().fill(ComuxLook.rule).frame(height: 1)
            }
            .padding(.horizontal, 12)
            .frame(height: ComuxLook.oneLineHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func agent(_ a: L0Agent, grouped: Bool) -> some View {
        let (glyph, color) = ComuxLook.status(a.status)
        return Button { act { session.jump(a.token) } } label: {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(glyph).foregroundStyle(color)
                    Text(grouped ? a.title : "\(a.space)/\(a.title)")
                        .font(a.focused ? ComuxLook.strong : ComuxLook.body)
                        .foregroundStyle(a.focused ? ComuxLook.text : ComuxLook.subtext).lineLimit(1)
                    Spacer(minLength: 4)
                    Text("\(a.status) \(elapsedLabel(since: a.since_unix))").foregroundStyle(color).lineLimit(1)
                }
                Text(a.detail ?? a.tool).foregroundStyle(ComuxLook.dim).lineLimit(1).padding(.leading, 16)
            }
            .font(ComuxLook.body)
            .padding(.horizontal, 12)
            .frame(height: ComuxLook.entryHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button("Jump to pane") { act { session.jump(a.token) } }
            Button("Rename tab…") { act { session.openOverlay("rename-tab", target: "\(a.space_id)/\(a.tab_id)") } }
        } preview: {
            DrawerPreview(session: session, pane: a.token)
        }
        .accessibilityIdentifier("agent-\(a.token)")
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

/// The right drawer: the notification center, newest first. A tap opens the notification (its
/// pane comes up and that pane's notifications go), ✕ or a swipe dismisses one.
struct NotificationsDrawer: View {
    @ObservedObject var session: MuxSession
    var act: (() -> Void) -> Void

    var body: some View {
        let items = session.model.notifications?.items ?? []
        VStack(spacing: 0) {
            HStack {
                Text("notifications").font(ComuxLook.strong).foregroundStyle(ComuxLook.dim)
                Spacer()
                Button("Clear all") { session.dismissNotification(nil) }
                    .font(ComuxLook.body).foregroundStyle(ComuxLook.subtext)
                    .disabled(items.isEmpty)
            }
            .padding(.horizontal, 12)
            .frame(height: 40)
            Rectangle().fill(ComuxLook.rule).frame(height: 1)
            if items.isEmpty {
                Text("no notifications").font(ComuxLook.body).foregroundStyle(ComuxLook.dim).padding(.top, 16)
                Spacer()
            } else {
                List {
                    ForEach(items) { row($0) }
                }
                .listStyle(.plain)
                .scrollContentBackground(.hidden)
            }
        }
        .frame(width: 300)
        .background(ComuxLook.base)
        .overlay(alignment: .leading) { Rectangle().fill(ComuxLook.rule).frame(width: 1) }
        .ignoresSafeArea(.keyboard)
        .accessibilityIdentifier("notificationsDrawer")
    }

    private func row(_ n: L0Notification) -> some View {
        let blocked = n.kind == "blocked"
        return HStack(alignment: .top, spacing: 8) {
            Button { act { session.openNotification(n) } } label: {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(blocked ? "▲" : "✓").foregroundStyle(blocked ? ComuxLook.yellow : ComuxLook.green)
                        Text("\(n.tool) · \(n.space)/\(n.tab)")
                            .font(n.read ? ComuxLook.body : ComuxLook.strong)
                            .foregroundStyle(n.read ? ComuxLook.subtext : ComuxLook.text).lineLimit(1)
                        Spacer(minLength: 4)
                        Text(n.when).foregroundStyle(ComuxLook.dim)
                    }
                    Text(n.body).foregroundStyle(ComuxLook.subtext).lineLimit(2).padding(.leading, 16)
                }
                .font(ComuxLook.body)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("notif-\(n.id)")
            Button { session.dismissNotification(n.id) } label: {
                Text("✕").font(ComuxLook.body).foregroundStyle(ComuxLook.dim).frame(width: 28, height: 28)
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("notif-dismiss-\(n.id)")
        }
        .padding(.vertical, 6)
        .listRowBackground(Color.clear)
        .listRowInsets(EdgeInsets(top: 0, leading: 12, bottom: 0, trailing: 8))
        .listRowSeparatorTint(ComuxLook.rule)
        .swipeActions { Button("Dismiss", role: .destructive) { session.dismissNotification(n.id) } }
    }
}

/// The current session's tabs, as the TUI's chips; the active one is scrolled into view.
struct TabStrip: View {
    @ObservedObject var session: MuxSession
    var select: (L0Tab) -> Void

    var body: some View {
        let tabs = session.model.activeSession?.tabs ?? []
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    ForEach(tabs) { tab in
                        let agent = tab.panes.contains { $0.kind == "agent" }
                        Button { select(tab) } label: {
                            HStack(spacing: 4) {
                                if agent { Text("●").foregroundStyle(tab.active ? ComuxLook.base : ComuxLook.yellow) }
                                Text("\(tab.index + 1) \(tab.title)").lineLimit(1)
                                if tab.bell { Text("!").font(ComuxLook.strong).foregroundStyle(tab.active ? ComuxLook.base : ComuxLook.red) }
                            }
                            .font(tab.active ? ComuxLook.strong : ComuxLook.body)
                            .foregroundStyle(tab.active ? ComuxLook.base : agent ? ComuxLook.yellow : ComuxLook.subtext)
                            .padding(.horizontal, 10).padding(.vertical, 6)
                            .background(RoundedRectangle(cornerRadius: 5).fill(tab.active ? ComuxLook.blue : ComuxLook.rule))
                        }
                        .id(tab.id)
                        .contextMenu {
                            if let sid = session.model.activeSession?.id {
                                Button("Rename tab…") { session.openOverlay("rename-tab", target: "\(sid)/\(tab.id)") }
                                Button("New tab") { session.newTab() }
                                Button("Close tab…", role: .destructive) {
                                    session.openOverlay("close-tab", target: "\(sid)/\(tab.id)")
                                }
                            }
                        }
                        .accessibilityIdentifier("tab-\(tab.index + 1)")
                    }
                }
            }
            .onChange(of: tabs.first(where: \.active)?.id) { _, id in
                if let id { withAnimation { proxy.scrollTo(id, anchor: .center) } }
            }
        }
    }
}

/// comux's status bar on the phone: session pill, the armed prefix, the outcome line, counters,
/// usage and host, in the TUI's colours.
struct StatusLine: View {
    @ObservedObject var session: MuxSession

    var body: some View {
        let st = session.model.status
        TimelineView(.periodic(from: .now, by: 30)) { _ in
            HStack(spacing: 8) {
                if st?.prefix_armed == true {
                    badge(st?.prefix_label ?? "^b", ComuxLook.yellow).accessibilityIdentifier("prefixArmed")
                }
                Text(session.model.activeSession?.name ?? "")
                    .font(ComuxLook.strong).foregroundStyle(ComuxLook.base)
                    .padding(.horizontal, 6).padding(.vertical, 1)
                    .background(RoundedRectangle(cornerRadius: 4).fill(ComuxLook.mauve))
                    .lineLimit(1)
                    .accessibilityIdentifier("activeSession")
                if let f = session.flash ?? st?.flash {
                    Text(f.text).foregroundStyle(f.error ? ComuxLook.red : ComuxLook.subtext).lineLimit(1)
                        .accessibilityIdentifier("flash")
                }
                Spacer(minLength: 4)
                HStack(spacing: 6) {
                    if let b = st?.bells, b > 0 { badge("! \(b)", ComuxLook.red) }
                    if let a = st?.attention, a > 0 { badge("⚑\(a)", ComuxLook.yellow) }
                    if let n = st?.agents, n > 0 { Text("● \(n)").foregroundStyle(ComuxLook.peach) }
                    if let u = st?.unread, u > 0 { Text("✉ \(u)").foregroundStyle(ComuxLook.blue) }
                    if let windows = st?.usage?.windows, !windows.isEmpty { ComuxUsage(windows: windows) }
                    if let up = st?.update { Text("⬆ \(up)").foregroundStyle(ComuxLook.green) }
                    if let h = st?.host, !h.isEmpty { Text(h).font(ComuxLook.strong).foregroundStyle(ComuxLook.text) }
                }
                // Horizontally compressible, so the usage readout's ViewThatFits sees the room
                // that is really left and steps down (resets → gauges → one window).
                .fixedSize(horizontal: false, vertical: true)
                .layoutPriority(1)
            }
            .font(ComuxLook.body)
            .padding(.horizontal, 10)
            .frame(height: 28)
            .background(ComuxLook.base)
            .overlay(alignment: .top) { Rectangle().fill(ComuxLook.rule).frame(height: 1) }
        }
    }

    private func badge(_ text: String, _ bg: Color) -> some View {
        Text(text).font(ComuxLook.strong).foregroundStyle(ComuxLook.base)
            .padding(.horizontal, 6).padding(.vertical, 1)
            .background(RoundedRectangle(cornerRadius: 4).fill(bg))
    }
}

/// Usage like the TUI's readout: per window a threshold-coloured gauge, the percentage and the
/// time to reset; short of room it drops the resets, then the gauges, then shows one window at a
/// time (tap for the next).
struct ComuxUsage: View {
    var windows: [L0UsageWindow]
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
        HStack(spacing: 8) {
            ForEach(Array(providers.enumerated()), id: \.offset) { _, p in
                HStack(spacing: 5) {
                    Text(p.name).foregroundStyle(ComuxLook.dim)
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
                if windows.count > 1 { Text("\(page % windows.count + 1)/\(windows.count)").foregroundStyle(ComuxLook.dim) }
            }
        }
        .buttonStyle(.plain)
        .fixedSize()
    }

    private func item(_ w: L0UsageWindow, label: String, gauge: Bool, reset: Bool) -> some View {
        let color = ComuxLook.usage(w.pct)
        return HStack(spacing: 4) {
            Text(label).foregroundStyle(ComuxLook.subtext)
            if gauge {
                ZStack(alignment: .leading) {
                    Capsule().fill(ComuxLook.rule)
                    Capsule().fill(color).frame(width: 40 * min(max(w.pct, 0), 100) / 100)
                }
                .frame(width: 40, height: 5)
            }
            Text("\(w.stale ? "~" : "")\(Int(w.pct.rounded()))%").foregroundStyle(color)
            if reset, let at = w.resets_at { Text("⟳ \(untilLabel(at))").foregroundStyle(ComuxLook.dim) }
        }
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
