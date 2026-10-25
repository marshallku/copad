import SwiftUI

/// A connected comux: the composed terminal, with comux's chrome as native UI — the tab strip
/// on top, spaces/agents in a left drawer, notifications in a right drawer, and the status
/// line (prefix, outcome, attention) at the bottom. comux stops drawing its own sidebar and
/// status bar once this app claims them (decisions #166); until then they stay in the frame.
struct SessionScreen: View {
    @ObservedObject var session: MuxSession
    @StateObject private var term: TerminalController
    @State private var leftOpen = false
    @State private var rightOpen = false
    @Environment(\.scenePhase) private var scenePhase
    var onClose: () -> Void
    var onTrustKey: (MuxHost) -> Void

    init(session: MuxSession, onClose: @escaping () -> Void, onTrustKey: @escaping (MuxHost) -> Void) {
        self.session = session
        _term = StateObject(wrappedValue: TerminalController(session: session))
        self.onClose = onClose
        self.onTrustKey = onTrustKey
    }

    var body: some View {
        ZStack {
            Color(uiColor: UIColor(rgb: Palette.defaultBG)).ignoresSafeArea()
            VStack(spacing: 0) {
                topBar
                TerminalRepresentable(controller: term)
                    .overlay { phaseOverlay }
                statusLine
            }
            drawers
        }
        .preferredColorScheme(.dark)
        .onAppear { if session.phase == .idle { session.connect() } }
        .onChange(of: scenePhase) { _, p in
            switch p {
            case .background:
                term.settleInput()
                session.suspend()
            case .active:
                if session.phase == .idle { session.connect() }
            default: break
            }
        }
        .onChange(of: session.phase) { _, p in if p == .live { term.focus() } }
        .sheet(item: Binding(get: { session.overlays.first }, set: { _ in })) { o in
            ComuxOverlayView(overlay: o) { choice, text in
                session.answerOverlay(o.overlay, choice: choice, text: text)
                term.focus()
            }
            .presentationDetents(o.isPicker ? [.medium, .large] : o.isMenu ? [.medium] : [.height(220)])
            .preferredColorScheme(.dark)
            .interactiveDismissDisabled()
        }
        .alert("Trust this host?", isPresented: Binding(
            get: { session.pendingHostKey != nil }, set: { if !$0 { session.pendingHostKey = nil } })) {
            Button("Trust") {
                if let h = session.trustPendingKey() {
                    onTrustKey(h)
                    session.connect()
                }
            }
            .accessibilityIdentifier("trustHostKey")
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("\(session.host.host) presents\n\(sshFingerprint(session.pendingHostKey ?? ""))")
        }
    }

    // MARK: chrome

    private var topBar: some View {
        HStack(spacing: 8) {
            Button { open(left: true) } label: {
                Image(systemName: "sidebar.left").padding(6)
            }
            .accessibilityIdentifier("openSpaces")
            TabStrip(session: session) { tab in
                navigate { session.selectTab(tab.id) }
            }
            Button { open(left: false) } label: {
                ZStack(alignment: .topTrailing) {
                    Image(systemName: "bell").padding(6)
                    if let n = session.model.notifications?.unread, n > 0 {
                        Text("\(n)").font(.caption2.bold()).padding(3)
                            .background(Circle().fill(.red)).offset(x: 4, y: -2)
                    }
                }
            }
            .accessibilityIdentifier("openNotifications")
            Menu {
                Button("New tab") { navigate { session.newTab() } }
                Button("New session…") { navigate { session.openOverlay("new-session") } }
                    .accessibilityIdentifier("menuNewSession")
                Button("Resume conversation…") { navigate { session.openOverlay("resume") } }
                    .accessibilityIdentifier("menuResume")
                Button("Reconnect") { session.connect() }
                Button("Disconnect", role: .destructive) { session.suspend(); onClose() }
            } label: { Image(systemName: "ellipsis.circle").padding(6) }
                .accessibilityIdentifier("sessionMenu")
        }
        .padding(.horizontal, 6)
        .frame(height: 40)
        .foregroundStyle(Color(uiColor: UIColor(rgb: Palette.defaultFG)))
    }

    private var statusLine: some View {
        let st = session.model.status
        return HStack(spacing: 10) {
            if st?.prefix_armed == true {
                Text(st?.prefix_label ?? "^b").font(.caption.monospaced().bold())
                    .padding(.horizontal, 5).background(Capsule().fill(Color.yellow.opacity(0.8)))
                    .foregroundStyle(.black)
                    .accessibilityIdentifier("prefixArmed")
            }
            Text(session.model.activeSession?.name ?? "")
                .font(.caption.bold())
                .accessibilityIdentifier("activeSession")
            if let f = session.flash ?? st?.flash {
                Text(f.text).font(.caption).foregroundStyle(f.error ? .red : .secondary).lineLimit(1)
                    .accessibilityIdentifier("flash")
            }
            Spacer()
            if let b = st?.bells, b > 0 {
                Text("!\(b)").font(.caption.bold()).foregroundStyle(.red)
            }
            if let u = st?.unread, u > 0 {
                Label("\(u)", systemImage: "envelope.fill").font(.caption2).foregroundStyle(.blue)
            }
            if let a = st?.attention, a > 0 {
                Label("\(a)", systemImage: "flag.fill").font(.caption).foregroundStyle(.orange)
            }
            if let n = st?.agents, n > 0 {
                Label("\(n)", systemImage: "circle.fill").font(.caption2).foregroundStyle(.yellow)
            }
            ForEach(Array((st?.usage?.windows ?? []).enumerated()), id: \.offset) { _, u in
                Text("\(u.stale ? "~" : "")\(u.provider) \(u.window) \(Int(u.pct))%").font(.caption2.monospaced())
                    .foregroundStyle(u.pct >= 90 ? .red : u.pct >= 70 ? .yellow : .green)
            }
            if let up = st?.update {
                Text("⬆ \(up)").font(.caption2).foregroundStyle(.green)
            }
            if let h = st?.host, !h.isEmpty {
                Text(h).font(.caption2.monospaced()).foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 10)
        .frame(height: 24)
        .foregroundStyle(Color(uiColor: UIColor(rgb: Palette.defaultFG)))
    }

    @ViewBuilder private var phaseOverlay: some View {
        switch session.phase {
        case .live: EmptyView()
        case let .connecting(msg):
            VStack { ProgressView(); Text(msg).font(.caption) }
                .padding().background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        case let .failed(why):
            VStack(spacing: 8) {
                Text(why).font(.callout).multilineTextAlignment(.center).accessibilityIdentifier("failure")
                Button("Reconnect") { session.connect() }.buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("reconnect")
            }
            .padding().background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10)).padding()
        case .idle:
            Button("Connect") { session.connect() }.buttonStyle(.borderedProminent)
        }
    }

    // MARK: drawers

    @ViewBuilder private var drawers: some View {
        if leftOpen || rightOpen {
            Color.black.opacity(0.35).ignoresSafeArea()
                .onTapGesture { withAnimation(.snappy) { leftOpen = false; rightOpen = false } }
        }
        HStack(spacing: 0) {
            if leftOpen {
                SpacesDrawer(session: session) { action in
                    withAnimation(.snappy) { leftOpen = false }
                    navigate(action)
                }
                .transition(.move(edge: .leading))
            }
            Spacer(minLength: 0)
            if rightOpen {
                NotificationsDrawer(session: session) { action in
                    withAnimation(.snappy) { rightOpen = false }
                    navigate(action)
                }
                .transition(.move(edge: .trailing))
            }
        }
        // Edge swipes, in thin strips so they never fight the terminal's own drag-to-scroll.
        HStack {
            Color.clear.frame(width: 16).contentShape(Rectangle())
                .gesture(DragGesture(minimumDistance: 20).onEnded { if $0.translation.width > 60 { open(left: true) } })
            Spacer()
            Color.clear.frame(width: 16).contentShape(Rectangle())
                .gesture(DragGesture(minimumDistance: 20).onEnded { if $0.translation.width < -60 { open(left: false) } })
        }
        .allowsHitTesting(!leftOpen && !rightOpen)
    }

    private func open(left: Bool) {
        term.settleInput()
        withAnimation(.snappy) {
            if left { leftOpen = true } else { rightOpen = true }
        }
    }

    /// Every navigation settles input first (codex C2), then goes over the render channel.
    private func navigate(_ action: () -> Void) {
        term.settleInput()
        action()
    }
}

/// The current session's tabs, as chips; the active one is scrolled into view.
struct TabStrip: View {
    @ObservedObject var session: MuxSession
    var select: (L0Tab) -> Void

    var body: some View {
        let tabs = session.model.activeSession?.tabs ?? []
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    ForEach(tabs) { tab in
                        Button { select(tab) } label: {
                            HStack(spacing: 4) {
                                if tab.panes.contains(where: { $0.kind == "agent" }) {
                                    Circle().fill(.yellow).frame(width: 6, height: 6)
                                }
                                Text("\(tab.index + 1) \(tab.title)").lineLimit(1)
                                if tab.bell { Text("!").bold().foregroundStyle(.red) }
                            }
                            .font(.caption.monospaced())
                            .padding(.horizontal, 8).padding(.vertical, 4)
                            .background(Capsule().fill(tab.active ? Color.accentColor.opacity(0.45) : Color.white.opacity(0.08)))
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

/// spaces (sessions) on top, every agent below with its status and time in it.
struct SpacesDrawer: View {
    @ObservedObject var session: MuxSession
    var act: (() -> Void) -> Void

    var body: some View {
        List {
            Section {
                ForEach(session.model.tree?.sessions ?? []) { s in
                    Button {
                        act { session.selectSession(s.id) }
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            HStack {
                                Text(s.name).fontWeight(s.active ? .bold : .regular)
                                Spacer()
                                Text("\(s.tabs.count)").font(.caption).foregroundStyle(.secondary)
                            }
                            if !s.branch.isEmpty {
                                Text(s.branch).font(.caption.monospaced()).foregroundStyle(.secondary)
                            }
                        }
                    }
                    .contextMenu {
                        Button("Rename session…") { act { session.openOverlay("rename-session", target: s.id) } }
                        Button("New session…") { act { session.openOverlay("new-session") } }
                        Button("Kill session…", role: .destructive) {
                            act { session.openOverlay("kill-session", target: s.id) }
                        }
                    }
                    .accessibilityIdentifier("space-\(s.name)")
                }
            } header: {
                HStack {
                    Text("spaces")
                    Spacer()
                    Button { act { session.openOverlay("new-session") } } label: { Image(systemName: "plus") }
                        .buttonStyle(.borderless).accessibilityIdentifier("drawerNewSession")
                    Button { act { session.openOverlay("resume") } } label: { Image(systemName: "clock.arrow.circlepath") }
                        .buttonStyle(.borderless).accessibilityIdentifier("drawerResume")
                }
            }
            Section("agents") {
                let agents = (session.model.agents?.agents ?? [])
                    .sorted { ($0.status == "blocked" ? 0 : 1) < ($1.status == "blocked" ? 0 : 1) }
                if agents.isEmpty { Text("no agents").foregroundStyle(.secondary) }
                ForEach(agents) { a in
                    Button { act { session.jump(a.token) } } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            HStack {
                                Circle().fill(color(a.status)).frame(width: 8, height: 8)
                                Text("\(a.space)/\(a.title)").lineLimit(1)
                                Spacer()
                                Text("\(a.status) \(elapsedLabel(since: a.since_unix))")
                                    .font(.caption.monospaced()).foregroundStyle(color(a.status))
                            }
                            Text(a.detail ?? a.tool).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        }
                    }
                    .contextMenu {
                        Button("Jump to pane") { act { session.jump(a.token) } }
                        Button("Rename tab…") { act { session.openOverlay("rename-tab", target: "\(a.space_id)/\(a.tab_id)") } }
                    }
                    .accessibilityIdentifier("agent-\(a.token)")
                }
            }
        }
        .frame(width: 300)
        .accessibilityIdentifier("spacesDrawer")
    }

    private func color(_ status: String) -> Color {
        switch status {
        case "blocked": return .orange
        case "working": return .yellow
        case "ready": return .green
        default: return .gray
        }
    }
}

/// The notification center: newest first; a tap jumps to the pane and marks it read.
struct NotificationsDrawer: View {
    @ObservedObject var session: MuxSession
    var act: (() -> Void) -> Void

    var body: some View {
        List {
            Section {
                ForEach(session.model.notifications?.items ?? []) { n in
                    Button {
                        act {
                            session.markRead(n.id)
                            if !n.token.isEmpty { session.jump(n.token) }
                        }
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            HStack {
                                Image(systemName: n.kind == "blocked" ? "exclamationmark.bubble" : "checkmark.circle")
                                    .foregroundStyle(n.kind == "blocked" ? .orange : .green)
                                Text("\(n.tool) · \(n.space)/\(n.tab)").lineLimit(1).fontWeight(n.read ? .regular : .bold)
                                Spacer()
                                Text(n.when).font(.caption).foregroundStyle(.secondary)
                            }
                            Text(n.body).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        }
                    }
                    .accessibilityIdentifier("notif-\(n.id)")
                }
            } header: {
                HStack {
                    Text("notifications")
                    Spacer()
                    Button("Mark all read") { act { session.markRead(nil) } }.font(.caption)
                }
            }
        }
        .frame(width: 300)
        .accessibilityIdentifier("notificationsDrawer")
    }
}

extension L0Overlay {
    var isPicker: Bool { if case .picker = spec { return true } else { return false } }
    var isMenu: Bool { if case .menu = spec { return true } else { return false } }
}
