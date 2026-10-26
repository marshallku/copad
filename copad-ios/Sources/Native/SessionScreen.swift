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
        .onAppear {
            PushRegistry.shared.visibleHost = session.host.id.uuidString
            if session.phase == .idle { session.connect() }
        }
        .onDisappear { PushRegistry.shared.visibleHost = nil }
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
                        Text("\(n)").font(ComuxLook.strong).foregroundStyle(ComuxLook.yellow)
                            .offset(x: 6, y: -2)
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
        .foregroundStyle(ComuxLook.subtext)
        .background(ComuxLook.base)
        .overlay(alignment: .bottom) { Rectangle().fill(ComuxLook.rule).frame(height: 1) }
    }

    private var statusLine: some View { StatusLine(session: session) }

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

extension L0Overlay {
    var isPicker: Bool { if case .picker = spec { return true } else { return false } }
    var isMenu: Bool { if case .menu = spec { return true } else { return false } }
}
