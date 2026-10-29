import SwiftUI

/// A comux overlay drawn natively (decisions #170, redrawn like the TUI in #180) — the `Ctrl-f`
/// switcher, the `Ctrl-b a` notification center, the `Ctrl-b C` / `Ctrl-b R` pickers, a
/// rename/new-session prompt, a kill confirm. Shared by the iOS app and copad on macOS.
/// Filtering happens here, so it is instant even when comux is far away; only opening, the
/// answer and the pane preview cross the network.

/// The look a host gives its overlays: its terminal font and theme, in the TUI's roles.
struct OverlayLook {
    var body: Font
    var strong: Font
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
    /// The terminal's default foreground and its 16 ANSI colours, for the preview.
    var fg: Color
    var ansi: [Color]

    func status(_ s: String) -> (glyph: String, color: Color) {
        switch s {
        case "working": return ("●", green)
        case "ready": return ("◐", blue)
        case "blocked": return ("▲", yellow)
        case "done": return ("●", green)
        default: return ("○", dim)
        }
    }

    /// green < 70% ≤ yellow < 90% ≤ red, like the TUI's gauges.
    func usage(_ pct: Double) -> Color { pct >= 90 ? red : pct >= 70 ? yellow : green }

    /// A preview colour: the theme for 0–15, xterm's cube and greys above, or rgb.
    func color(_ c: L0RunColor) -> Color {
        switch c {
        case let .rgb(v): return Color(red: Double(v >> 16 & 0xFF) / 255, green: Double(v >> 8 & 0xFF) / 255,
                                       blue: Double(v & 0xFF) / 255)
        case let .indexed(i):
            if i < 16 { return ansi.indices.contains(i) ? ansi[i] : fg }
            if i < 232 {
                let n = i - 16
                let step = { (x: Int) in x == 0 ? 0.0 : Double(55 + x * 40) / 255 }
                return Color(red: step(n / 36), green: step(n / 6 % 6), blue: step(n % 6))
            }
            let g = Double(8 + (min(i, 255) - 232) * 10) / 255
            return Color(red: g, green: g, blue: g)
        }
    }
}

/// What the cards and the preview read besides the overlay itself: the live topics, so a row
/// shows an agent's status as it is now, not as it was when the picker opened.
struct OverlayContext {
    var agents: [L0Agent] = []
    var sessions: [L0Session] = []
    var preview: L0PreviewState?
    /// A phone held upright: the panel docks to the bottom and previews on long-press.
    var compact = false

    /// The live tab a tab-overview row (`t:<session>/<tab>`) names.
    func tab(for row: L0PickerRow) -> L0Tab? {
        guard row.id.hasPrefix("t:") else { return nil }
        let parts = row.id.dropFirst(2).split(separator: "/", maxSplits: 1).map(String.init)
        guard parts.count == 2 else { return nil }
        return sessions.first { $0.id == parts[0] }?.tabs.first { $0.id == parts[1] }
    }
}

/// The host's side of an overlay.
struct OverlayHandlers {
    /// (choice, text): both nil = cancelled. Closes the overlay.
    var answer: (String?, String?) -> Void
    /// A staying choice: the overlay stays until comux re-sends it.
    var act: (String) -> Void
    /// Keep the preview of a pane (rows × cols) fresh; nil stops it.
    var preview: (String?, Int, Int) -> Void
}

/// The overlay layer a host puts over its whole content: a light scrim that swallows clicks
/// (outside the card = cancel) and the card, centered like the TUI's box.
struct ComuxOverlayLayer: View {
    let overlay: L0Overlay
    let look: OverlayLook
    let context: OverlayContext
    let handlers: OverlayHandlers

    var body: some View {
        GeometryReader { geo in
            let size = cardSize(geo.size)
            ZStack(alignment: context.compact ? .bottom : .center) {
                Color.black.opacity(0.18)
                    .contentShape(Rectangle())
                    .onTapGesture { handlers.answer(nil, nil) }
                ComuxOverlayView(overlay: overlay, look: look, context: context, handlers: handlers,
                                 wide: size.width >= 700)
                    .frame(width: size.width, height: size.height)
                    .background(RoundedRectangle(cornerRadius: 8).fill(look.base.opacity(0.97)))
                    .overlay(RoundedRectangle(cornerRadius: 8).stroke(look.accent.opacity(0.9), lineWidth: 1))
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .shadow(color: .black.opacity(0.35), radius: 18, y: 6)
                    .padding(.bottom, context.compact ? 8 : 0)
            }
            .frame(width: geo.size.width, height: geo.size.height)
        }
    }

    /// The TUI's rule: a picker with a preview takes most of the frame, the rest stay small.
    private func cardSize(_ s: CGSize) -> CGSize {
        if context.compact {
            let h: CGFloat
            switch overlay.spec {
            case .picker: h = s.height * 0.75
            case .menu: h = min(s.height * 0.6, 420)
            default: h = 200
            }
            return CGSize(width: s.width - 16, height: h)
        }
        switch overlay.spec {
        case let .picker(p):
            let previews = p.tabs.contains { t in t.rows.contains { !($0.pane ?? "").isEmpty } }
            if previews && s.width >= 820 && s.height >= 420 {
                return CGSize(width: s.width * 0.9, height: s.height * 0.8)
            }
            return CGSize(width: min(max(s.width * 0.6, 420), s.width - 32), height: min(max(s.height * 0.6, 320), s.height - 32))
        case let .menu(_, items):
            return CGSize(width: min(320, s.width - 32), height: min(CGFloat(items.count + 2) * (look.lineHeight + 14) + 40, s.height - 32))
        default:
            return CGSize(width: min(460, s.width - 32), height: 170)
        }
    }
}

struct ComuxOverlayView: View {
    let overlay: L0Overlay
    let look: OverlayLook
    let context: OverlayContext
    let handlers: OverlayHandlers
    var wide = true

    var body: some View {
        switch overlay.spec {
        case let .picker(p):
            PickerOverlay(picker: p, look: look, context: context, handlers: handlers, wide: wide)
                .id(overlay.overlay)
        case let .menu(title, items):
            MenuOverlay(title: title, items: items, look: look, answer: handlers.answer)
        case .unsupported:
            Color.clear.frame(width: 1, height: 1).onAppear { handlers.answer(nil, nil) }
        case let .prompt(title, initial, placeholder):
            PromptOverlay(title: title, initial: initial, placeholder: placeholder, look: look, answer: handlers.answer)
        case let .confirm(message):
            ConfirmOverlay(message: message, look: look, answer: handlers.answer)
        }
    }
}

// MARK: - keys

/// Focus once the overlay is in its window: the host makes the layer first responder as it
/// adds it, and a focus set before that is lost.
@MainActor
func focusSoon(_ set: () -> Void) async {
    try? await Task.sleep(for: .milliseconds(60))
    set()
}

/// A key press in comux's naming (`Left`, `C-r`, `d`); nil for what the overlay leaves alone
/// (⌘ shortcuts).
func comuxKeyName(_ press: KeyPress) -> String? {
    switch press.key {
    case .leftArrow: return "Left"
    case .rightArrow: return "Right"
    case .upArrow: return "Up"
    case .downArrow: return "Down"
    case .return: return "Enter"
    case .escape: return "Esc"
    case .tab: return "Tab"
    default: break
    }
    if press.modifiers.contains(.command) { return nil }
    if press.modifiers.contains(.control) {
        guard var v = press.key.character.unicodeScalars.first?.value else { return nil }
        if v < 0x20 { v += 0x60 } // ^R arrives as 0x12
        guard let s = Unicode.Scalar(v) else { return nil }
        return "C-" + String(Character(s)).lowercased()
    }
    return press.characters.count == 1 ? press.characters : nil
}

/// What a key does in a picker (pure, so it is unit-tested).
enum PickerKeyOutcome: Equatable {
    case cancel
    case answer(String)
    case act(String)
    case tab(Int)
    case move(Int)
    case pick
    case none
}

enum PickerLogic {
    /// The TUI's order: a declared key wins; then ←/→ (and Tab) switch tabs, ↑/↓ ^n/^p move,
    /// Enter picks, Esc cancels. A printable declared key only counts without a filter field —
    /// with one, typing belongs to the filter.
    static func outcome(_ key: String, picker: L0Picker, row: L0PickerRow?) -> PickerKeyOutcome {
        if key == "Esc" { return .cancel }
        let typed = key.count == 1
        if !typed || picker.noFilter {
            if let k = picker.keys.first(where: { $0.key == key }) {
                let c = k.choice ?? ""
                if c.isEmpty { return .cancel }
                return k.stays == true ? .act(c) : .answer(c)
            }
            if let row, row.no_actions != true, let a = picker.actions.first(where: { $0.key == key }) {
                let c = "\(a.id):\(row.id)"
                return a.stays == true ? .act(c) : .answer(c)
            }
        }
        switch key {
        case "Left": return picker.tabs.count > 1 ? .tab(-1) : .none
        case "Right", "Tab": return picker.tabs.count > 1 ? .tab(1) : .none
        case "Up", "C-p": return .move(-1)
        case "Down", "C-n": return .move(1)
        case "Enter": return .pick
        default: break
        }
        if picker.noFilter {
            switch key {
            case "k": return .move(-1)
            case "j": return .move(1)
            case "q": return .cancel
            default: break
            }
        }
        return .none
    }

    /// `!` working, `@` blocked, `#` ready/idle — the TUI switcher's status filters.
    static func statusOp(_ filter: String) -> (Set<String>?, String) {
        let ops: [Character: Set<String>] = ["!": ["working"], "@": ["blocked"], "#": ["ready", "idle"]]
        guard let first = filter.first, let op = ops[first] else { return (nil, filter) }
        return (op, String(filter.dropFirst()).trimmingCharacters(in: .whitespaces))
    }

    /// Rows matching `filter`: contiguous matches before scattered ones (comux's own ranking),
    /// over the label and detail; a status op keeps rows whose agents (`statuses`) have one.
    static func filter(_ rows: [L0PickerRow], _ filter: String,
                       statuses: (L0PickerRow) -> Set<String>?) -> [L0PickerRow] {
        let (op, rest) = statusOp(filter)
        var all = rows
        if let op {
            all = all.filter { r in statuses(r).map { !$0.isDisjoint(with: op) } ?? false }
        }
        guard !rest.isEmpty else { return all }
        let f = rest.lowercased()
        let hay = { (r: L0PickerRow) in (r.label + " " + (r.detail ?? "")).lowercased() }
        let contig = all.filter { hay($0).contains(f) }
        let subseq = all.filter { r in !contig.contains(r) && isSubsequence(f, of: hay(r)) }
        return contig + subseq
    }
}

// MARK: - picker

private struct PickerOverlay: View {
    let picker: L0Picker
    let look: OverlayLook
    let context: OverlayContext
    let handlers: OverlayHandlers
    let wide: Bool
    @State private var tab = 0
    @State private var filter = ""
    /// The highlighted row (by id, so a live re-send keeps it): Enter picks it.
    @State private var sel: String?
    @State private var lastIndex = 0
    /// The filter field has the keys — or, without one, the panel itself.
    @FocusState private var fieldFocused: Bool
    @FocusState private var panelFocused: Bool

    private var tabs: [L0PickerTab] { picker.tabs }

    private var rows: [L0PickerRow] {
        guard tabs.indices.contains(tab) else { return [] }
        let all = tabs[tab].rows
        let joins = all.contains { !($0.pane ?? "").isEmpty || !($0.session ?? "").isEmpty }
        return PickerLogic.filter(all, filter) { r in joins ? statuses(r) : nil }
    }

    private func statuses(_ r: L0PickerRow) -> Set<String> {
        // A tab: every agent in any of its panes, not just the focused one the preview shows.
        if let t = context.tab(for: r) {
            let tokens = Set(t.panes.map(\.token))
            return Set(context.agents.filter { tokens.contains($0.token) }.map(\.status))
        }
        if let s = r.session, !s.isEmpty {
            return Set(context.agents.filter { $0.space_id == s }.map(\.status))
        }
        return Set(context.agents.filter { $0.token == r.pane }.map(\.status))
    }

    /// The row Enter picks: the highlighted one if it is still listed, else the one now at its
    /// old place (a live re-send removed it), else the first.
    private var current: L0PickerRow? {
        let r = rows
        if let s = sel, let row = r.first(where: { $0.id == s }) { return row }
        return r.isEmpty ? nil : r[min(lastIndex, r.count - 1)]
    }

    private var showsPreview: Bool {
        wide && !context.compact && rows.contains { !($0.pane ?? "").isEmpty }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            if tabs.count > 1 { tabChips }
            if !picker.noFilter { filterLine }
            if !picker.note.isEmpty {
                Text(picker.note).font(look.body).foregroundStyle(look.peach).padding(.horizontal, 12).padding(.bottom, 4)
            }
            Rectangle().fill(look.rule).frame(height: 1)
            GeometryReader { geo in
                HStack(spacing: 0) {
                    list.frame(width: showsPreview ? max(geo.size.width * 0.4, min(30 * look.advance, geo.size.width)) : geo.size.width)
                    if showsPreview {
                        Rectangle().fill(look.rule).frame(width: 1)
                        PanePreview(pane: current?.pane, look: look, state: context.preview, handlers: handlers)
                    }
                }
            }
        }
        .font(look.body)
        .foregroundStyle(look.text)
        .focusable(picker.noFilter)
        .focused($panelFocused)
        .focusEffectDisabled()
        .onKeyPress(phases: .down) { press in picker.noFilter ? key(press) : .ignored }
        .onAppear {
            tab = min(max(picker.tab, 0), max(tabs.count - 1, 0))
            sel = picker.selected ?? rows.first?.id
        }
        .task { await focusSoon { if picker.noFilter { panelFocused = true } else { fieldFocused = true } } }
        .onDisappear { handlers.preview(nil, 0, 0) }
        // The same overlay re-sent with its rows (decisions #172): take its default row now —
        // the one chosen while it was still loading pointed at nothing (or the toggle).
        .onChange(of: picker.loading) { _, loading in
            if !loading { sel = picker.selected ?? rows.first?.id }
        }
        .onChange(of: sel) { _, _ in
            if let i = rows.firstIndex(where: { $0.id == sel }) { lastIndex = i }
        }
    }

    private var hints: String {
        var h = ["↵ go"]
        if tabs.count > 1 { h.append("←/→ tab") }
        for a in picker.actions where !(a.key ?? "").isEmpty { h.append("\(keyLabel(a.key!)) \(a.label.lowercased())") }
        for k in picker.keys where !(k.choice ?? "").isEmpty { h.append("\(keyLabel(k.key)) \(k.choice!.replacingOccurrences(of: "-", with: " "))") }
        h.append("esc")
        return h.joined(separator: " · ")
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text(picker.title).font(look.strong).foregroundStyle(look.accent).lineLimit(1).truncationMode(.head)
            if picker.loading {
                ProgressView().controlSize(.small).accessibilityIdentifier("overlayLoading")
            }
            Spacer(minLength: 8)
            if !context.compact {
                Text(hints).foregroundStyle(look.dim).lineLimit(1).truncationMode(.tail)
            }
            Button { handlers.answer(nil, nil) } label: {
                Text("✕").foregroundStyle(look.dim)
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("overlayCancel")
        }
        .padding(.horizontal, 12)
        .frame(height: look.lineHeight + 14)
    }

    private var tabChips: some View {
        HStack(spacing: 6) {
            ForEach(tabs.indices, id: \.self) { i in
                Button { tab = i; sel = rows.first?.id } label: {
                    Text(" \(tabs[i].name) ")
                        .font(look.strong)
                        .foregroundStyle(i == tab ? look.base : look.dim)
                        .padding(.vertical, 2)
                        .background(RoundedRectangle(cornerRadius: 3).fill(i == tab ? look.accent : look.rule.opacity(0.6)))
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("overlayTab-\(tabs[i].name)")
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
    }

    private var filterLine: some View {
        HStack(spacing: 6) {
            Text(">").font(look.strong).foregroundStyle(look.accent)
            TextField("", text: $filter, prompt: Text("type to filter").foregroundStyle(look.dim))
                .textFieldStyle(.plain)
                .font(look.body)
                .foregroundStyle(look.text)
                .autocorrectionDisabled()
                #if os(iOS)
                .textInputAutocapitalization(.never)
                #endif
                .focused($fieldFocused)
                .onSubmit { pick(current) }
                .onKeyPress(phases: .down) { press in key(press) }
                .onChange(of: filter) { _, _ in if !rows.contains(where: { $0.id == sel }) { sel = rows.first?.id } }
                .accessibilityIdentifier("overlayFilter")
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 8)
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                TimelineView(.periodic(from: .now, by: 30)) { tl in
                    LazyVStack(spacing: 2) {
                        if rows.isEmpty && !picker.loading {
                            Text("(no match)").foregroundStyle(look.dim).frame(maxWidth: .infinity, alignment: .leading)
                                .padding(12)
                        }
                        ForEach(rows) { r in
                            card(r, now: tl.date).id(r.id)
                        }
                    }
                    .padding(6)
                }
            }
            .onChange(of: sel) { _, s in if let s { withAnimation(.easeOut(duration: 0.1)) { proxy.scrollTo(s) } } }
        }
    }

    @ViewBuilder private func card(_ r: L0PickerRow, now: Date) -> some View {
        let on = r.id == current?.id
        let content = PickerCard(row: r, look: look, context: context, now: now, selected: on)
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 5).fill(on ? look.hover : .clear))
            .overlay(alignment: .leading) {
                if on { RoundedRectangle(cornerRadius: 1).fill(look.accent).frame(width: 3).padding(.vertical, 4) }
            }
            .contentShape(Rectangle())
        Button { pick(r) } label: { content }
            .buttonStyle(.plain)
            #if os(macOS)
            .onHover { if $0 { sel = r.id } }
            .contextMenu { rowActions(r) }
            #else
            // No room for a side preview on a phone: long-press shows the pane instead.
            .contextMenu {
                rowActions(r)
            } preview: {
                if let pane = r.pane, !pane.isEmpty {
                    PanePreview(pane: pane, look: look, state: context.preview, handlers: handlers)
                        .frame(width: 360, height: 300)
                        .background(look.base)
                } else {
                    PickerCard(row: r, look: look, context: context, now: now, selected: false)
                        .padding(10).background(look.base)
                }
            }
            #endif
            .accessibilityIdentifier("overlayRow-\(r.id)")
    }

    @ViewBuilder private func rowActions(_ r: L0PickerRow) -> some View {
        if r.no_actions != true {
            ForEach(picker.actions, id: \.id) { a in
                Button(a.label) { run(a.stays == true ? .act("\(a.id):\(r.id)") : .answer("\(a.id):\(r.id)")) }
            }
        }
    }

    private func pick(_ r: L0PickerRow?) {
        if let r { handlers.answer(r.id, nil) }
    }

    private func key(_ press: KeyPress) -> KeyPress.Result {
        guard let name = comuxKeyName(press) else { return .ignored }
        let o = PickerLogic.outcome(name, picker: picker, row: current)
        if o == .none { return .ignored }
        run(o)
        return .handled
    }

    private func run(_ o: PickerKeyOutcome) {
        switch o {
        case .cancel: handlers.answer(nil, nil)
        case let .answer(c): handlers.answer(c, nil)
        case let .act(c): handlers.act(c)
        case let .tab(d):
            tab = (tab + d + tabs.count) % max(tabs.count, 1)
            sel = rows.first?.id
        case let .move(d):
            let r = rows
            guard !r.isEmpty else { return }
            let i = r.firstIndex(where: { $0.id == current?.id }) ?? 0
            sel = r[(i + d + r.count) % r.count].id
        case .pick: pick(current)
        case .none: break
        }
    }
}

/// `C-r` → `^R`, `Right` → `→`.
private func keyLabel(_ k: String) -> String {
    if k.hasPrefix("C-") { return "^" + k.dropFirst(2).uppercased() }
    switch k {
    case "Left": return "←"
    case "Right": return "→"
    default: return k
    }
}

/// One row as a card: a session or an agent drawn from the live topics (glyph, status and time
/// as they are now), anything else as its label and detail.
private struct PickerCard: View {
    let row: L0PickerRow
    let look: OverlayLook
    let context: OverlayContext
    let now: Date
    let selected: Bool

    var body: some View {
        if let s = row.session.flatMap({ id in context.sessions.first { $0.id == id } }) {
            session(s)
        } else if row.id.hasPrefix("a:"), let a = context.agents.first(where: { $0.token == row.pane }) {
            agent(a)
        } else if let t = context.tab(for: row) {
            tab(t)
        } else {
            plain
        }
    }

    private func session(_ s: L0Session) -> some View {
        let agents = context.agents.filter { $0.space_id == s.id }
        let color = s.active ? look.mauve : agents.isEmpty ? look.dim : look.peach
        let sub = [s.subtitle.trimmingCharacters(in: .whitespaces), "\(s.tabs.count) tab\(s.tabs.count == 1 ? "" : "s")", shortDir(s.dir)]
            .filter { !$0.isEmpty }.joined(separator: " · ")
        return VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text("▪").foregroundStyle(color)
                Text(s.name).font(look.strong).foregroundStyle(s.active ? look.mauve : look.text).lineLimit(1)
                Spacer(minLength: 4)
                HStack(spacing: 2) {
                    ForEach(Array(agents.enumerated()), id: \.offset) { _, a in
                        Text(look.status(a.status).glyph).foregroundStyle(look.status(a.status).color)
                    }
                }
            }
            Text(sub).foregroundStyle(look.dim).lineLimit(1).truncationMode(.middle).padding(.leading, 14)
        }
    }

    private func agent(_ a: L0Agent) -> some View {
        let st = look.status(a.status)
        let since = elapsedLabel(since: a.since_unix, now: now)
        let place = a.title == a.space ? a.space : "\(a.space) · \(a.title)"
        return VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(st.glyph).foregroundStyle(st.color)
                Text(a.tool).font(look.strong).lineLimit(1)
                Text(since.isEmpty ? a.status : "\(a.status) \(since)").foregroundStyle(st.color).lineLimit(1)
                Spacer(minLength: 4)
                if a.focused { Text("here").foregroundStyle(look.dim) }
            }
            Text(a.detail.map { "\(place) — \($0)" } ?? place)
                .foregroundStyle(look.dim).lineLimit(1).truncationMode(.tail).padding(.leading, 14)
        }
    }

    /// A tab as its chip reads (`2 api`), with each pane's program and, for an agent, its
    /// status as it is now.
    private func tab(_ t: L0Tab) -> some View {
        let agents = t.panes.compactMap { p in context.agents.first { $0.token == p.token } }
        let glyph = t.bell ? ("!", look.red) : !agents.isEmpty ? ("●", look.yellow) : ("▪", t.active ? look.mauve : look.dim)
        let panes = t.panes.map { p -> String in
            let label = p.label.isEmpty ? "shell" : p.label
            guard let a = context.agents.first(where: { $0.token == p.token }) else { return label }
            return "\(label) \(a.status)"
        }.joined(separator: " · ")
        return VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(glyph.0).foregroundStyle(glyph.1)
                Text("\(t.index + 1) \(t.title)").font(look.strong)
                    .foregroundStyle(t.active ? look.mauve : look.text).lineLimit(1)
                Spacer(minLength: 4)
                HStack(spacing: 2) {
                    ForEach(Array(agents.enumerated()), id: \.offset) { _, a in
                        Text(look.status(a.status).glyph).foregroundStyle(look.status(a.status).color)
                    }
                }
                if t.active { Text("here").foregroundStyle(look.dim) }
            }
            Text(panes).foregroundStyle(look.dim).lineLimit(1).truncationMode(.tail).padding(.leading, 14)
        }
    }

    private var plain: some View {
        let badge = row.badge ?? ""
        let st = look.status(badge)
        return VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                if !badge.isEmpty && badge != "live" { Text(st.glyph).foregroundStyle(st.color) }
                Text(row.label).font(row.current == true ? look.strong : look.body).lineLimit(1)
                Spacer(minLength: 4)
                if !badge.isEmpty { Text(badge).foregroundStyle(badge == "live" ? look.green : st.color) }
            }
            if let d = row.detail, !d.isEmpty {
                Text(d).foregroundStyle(look.dim).lineLimit(2).truncationMode(.tail)
                    .padding(.leading, badge.isEmpty || badge == "live" ? 0 : 14)
            }
        }
    }
}

private func shortDir(_ d: String) -> String {
    let home = NSHomeDirectory()
    return d.hasPrefix(home) ? "~" + d.dropFirst(home.count) : d
}

// MARK: - preview

/// The selected row's pane, as comux renders it (`pane-preview`), bottom-anchored and sized to
/// this box; refreshed by the host while shown.
struct PanePreview: View {
    let pane: String?
    let look: OverlayLook
    let state: L0PreviewState?
    let handlers: OverlayHandlers

    var body: some View {
        GeometryReader { geo in
            let rows = max(Int(geo.size.height / look.lineHeight) - 1, 1)
            let cols = max(Int((geo.size.width - 20) / look.advance), 1)
            VStack(alignment: .leading, spacing: 0) {
                content(rows: rows)
            }
            .padding(10)
            .frame(width: geo.size.width, height: geo.size.height, alignment: .topLeading)
            .clipped()
            .onAppear { handlers.preview(pane, rows, cols) }
            .onChange(of: pane) { _, p in handlers.preview(p, rows, cols) }
            .onChange(of: geo.size) { _, _ in handlers.preview(pane, rows, cols) }
        }
        .accessibilityIdentifier("overlayPreview")
    }

    @ViewBuilder private func content(rows: Int) -> some View {
        let p = state?.preview.flatMap { $0.pane == pane ? $0 : nil }
        if pane == nil {
            note("nothing to preview")
        } else if state?.state == "unsupported" {
            note("preview needs a newer comux")
        } else if state?.state == "gone" {
            note("that pane has closed")
        } else if let p {
            Text(p.header ?? "").font(look.strong).foregroundStyle(look.subtext).lineLimit(1)
                .frame(height: look.lineHeight)
            Spacer(minLength: 0)
            ForEach(Array(p.lines.suffix(rows).enumerated()), id: \.offset) { _, line in
                Text(attributed(line)).lineLimit(1).fixedSize(horizontal: true, vertical: false)
                    .frame(height: look.lineHeight, alignment: .leading)
            }
        } else {
            ProgressView().controlSize(.small)
        }
    }

    private func note(_ s: String) -> some View {
        Text(s).foregroundStyle(look.dim).font(look.body)
    }

    private func attributed(_ line: [L0Run]) -> AttributedString {
        var out = AttributedString()
        for r in line {
            var a = AttributedString(r.t)
            var fg = r.fg.map(look.color) ?? look.fg
            var bg = r.bg.map(look.color)
            if r.reverse == true {
                let f = fg
                fg = bg ?? look.base
                bg = f
            }
            a.foregroundColor = r.dim == true ? fg.opacity(0.6) : fg
            if let bg { a.backgroundColor = bg }
            var font = r.bold == true ? look.strong : look.body
            if r.italic == true { font = font.italic() }
            a.font = font
            if r.underline == true { a.underlineStyle = .single }
            out += a
        }
        return out
    }
}

// MARK: - prompt / confirm / menu

private struct PromptOverlay: View {
    let title: String
    let initial: String
    let placeholder: String
    let look: OverlayLook
    let answer: (String?, String?) -> Void
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(look.strong).foregroundStyle(look.accent)
            HStack(spacing: 6) {
                Text(">").font(look.strong).foregroundStyle(look.accent)
                TextField("", text: $text, prompt: Text(placeholder.isEmpty ? title : placeholder).foregroundStyle(look.dim))
                    .textFieldStyle(.plain)
                    .font(look.body)
                    .foregroundStyle(look.text)
                    .autocorrectionDisabled()
                    #if os(iOS)
                    .textInputAutocapitalization(.never)
                    #endif
                    .focused($focused)
                    .onSubmit { answer(nil, text) }
                    .onKeyPress(.escape) { answer(nil, nil); return .handled }
                    // The TUI's prompt cancels on ^C too.
                    .onKeyPress(phases: .down) { press in
                        guard comuxKeyName(press) == "C-c" else { return .ignored }
                        answer(nil, nil)
                        return .handled
                    }
                    .accessibilityIdentifier("overlayText")
            }
            .padding(8)
            .background(RoundedRectangle(cornerRadius: 4).fill(look.rule.opacity(0.5)))
            HStack {
                Spacer()
                OverlayButton(label: "Cancel", look: look, color: look.dim) { answer(nil, nil) }
                    .accessibilityIdentifier("overlayCancel")
                OverlayButton(label: "OK", look: look, color: look.accent, filled: true) { answer(nil, text) }
                    .accessibilityIdentifier("overlayOK")
            }
        }
        .padding(16)
        .font(look.body)
        .onAppear { text = initial }
        .task { await focusSoon { focused = true } }
    }
}

private struct ConfirmOverlay: View {
    let message: String
    let look: OverlayLook
    let answer: (String?, String?) -> Void
    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 16) {
            Text(message).font(look.strong).foregroundStyle(look.text).multilineTextAlignment(.center)
            Text("y = yes · n / esc = no").foregroundStyle(look.dim)
            HStack(spacing: 10) {
                OverlayButton(label: "Cancel", look: look, color: look.dim) { answer(nil, nil) }
                    .accessibilityIdentifier("overlayCancel")
                OverlayButton(label: "Confirm", look: look, color: look.red, filled: true) { answer("yes", nil) }
                    .accessibilityIdentifier("overlayConfirm")
            }
        }
        .padding(18)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .font(look.body)
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        // The TUI's y/n.
        .onKeyPress(phases: .down) { press in
            switch comuxKeyName(press) {
            case "y", "Y": answer("yes", nil); return .handled
            case "n", "N", "Esc", "q": answer(nil, nil); return .handled
            default: return .ignored
            }
        }
        .task { await focusSoon { focused = true } }
    }
}

private struct MenuOverlay: View {
    let title: String
    let items: [L0MenuItem]
    let look: OverlayLook
    let answer: (String?, String?) -> Void
    @State private var sel = 0
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            if !title.isEmpty {
                Text(title).font(look.strong).foregroundStyle(look.accent).padding(.horizontal, 10).padding(.bottom, 6)
            }
            ForEach(Array(items.enumerated()), id: \.offset) { i, item in
                Button { answer(item.id, nil) } label: {
                    Text(item.label)
                        .foregroundStyle(item.destructive == true ? look.red : look.text)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 10).padding(.vertical, 6)
                        .background(RoundedRectangle(cornerRadius: 4).fill(i == sel ? look.hover : .clear))
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("overlayMenu-\(item.label)")
            }
            Spacer(minLength: 0)
        }
        .padding(10)
        .font(look.body)
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onKeyPress(phases: .down) { press in
            switch comuxKeyName(press) {
            case "Up", "k": sel = (sel - 1 + items.count) % max(items.count, 1); return .handled
            case "Down", "j": sel = (sel + 1) % max(items.count, 1); return .handled
            case "Enter": if items.indices.contains(sel) { answer(items[sel].id, nil) }; return .handled
            case "Esc", "q": answer(nil, nil); return .handled
            default: return .ignored
            }
        }
        .task { await focusSoon { focused = true } }
    }
}

private struct OverlayButton: View {
    let label: String
    let look: OverlayLook
    let color: Color
    var filled = false
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(" \(label) ")
                .font(look.strong)
                .foregroundStyle(filled ? look.base : color)
                .padding(.horizontal, 8).padding(.vertical, 4)
                .background(RoundedRectangle(cornerRadius: 4).fill(filled ? color : look.rule.opacity(0.6)))
        }
        .buttonStyle(.plain)
    }
}

private func isSubsequence(_ needle: String, of hay: String) -> Bool {
    var it = hay.makeIterator()
    for ch in needle {
        var found = false
        while let h = it.next() { if h == ch { found = true; break } }
        if !found { return false }
    }
    return true
}

// MARK: - usage readout (decisions #179, #182)

/// The status bar's usage readout, as the TUI draws it: `inline` shows every window and sheds
/// detail when short of room; `paged` is the TUI's carousel — one page at a time with `n/N`,
/// rotated by comux (`usage_rotate_secs`), a click pages it for everyone (`usage-page`). Its
/// width is the widest page's, so the chips beside it never shift as it turns.
struct ComuxUsageView: View {
    var usage: L0Usage
    var look: OverlayLook
    var gaugeWidth: CGFloat = 56
    /// Page the carousel by a delta (through comux, so every view of it turns together).
    var turn: (Int) -> Void

    private var windows: [L0UsageWindow] { usage.windows }
    private var bars: Bool { usage.gauge != "text" }

    var body: some View {
        if usage.layout == "paged", let pages = usage.pages, !pages.isEmpty {
            ViewThatFits(in: .horizontal) {
                carousel(pages, gauge: bars)
                carousel(pages, gauge: false)
            }
        } else {
            ViewThatFits(in: .horizontal) {
                row(gauge: bars, reset: true)
                row(gauge: bars, reset: false)
                row(gauge: false, reset: false)
                single
            }
        }
    }

    private func carousel(_ pages: [L0UsagePage], gauge: Bool) -> some View {
        let current = (usage.page ?? 0) % pages.count
        return Button { turn(1) } label: {
            HStack(spacing: 6) {
                ZStack(alignment: .leading) {
                    ForEach(Array(pages.enumerated()), id: \.offset) { i, p in
                        page(p, gauge: gauge).opacity(i == current ? 1 : 0)
                    }
                }
                if pages.count > 1 { Text("\(current + 1)/\(pages.count)").foregroundStyle(look.dim) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .fixedSize()
        .accessibilityIdentifier("usagePage-\(current + 1)")
    }

    private func page(_ p: L0UsagePage, gauge: Bool) -> some View {
        let ws = p.windows.filter { windows.indices.contains($0) }.map { windows[$0] }
        return HStack(spacing: 6) {
            ForEach(Array(ws.enumerated()), id: \.offset) { i, w in
                if i > 0 { Text("·").foregroundStyle(look.dim) }
                let label = "\(w.stale ? "~" : "")\(w.provider) \(w.window)"
                switch p.kind {
                case "reset":
                    HStack(spacing: 4) {
                        Text(label).foregroundStyle(look.subtext)
                        Text("⟳ \(w.resetLabel ?? "–")").foregroundStyle(look.dim)
                    }
                case "usage":
                    item(w, label: label, gauge: gauge, reset: false)
                default:
                    item(w, label: label, gauge: gauge, reset: true)
                }
            }
        }
    }

    private func row(gauge: Bool, reset: Bool) -> some View {
        HStack(spacing: 10) {
            ForEach(Array(providers.enumerated()), id: \.offset) { _, p in
                HStack(spacing: 6) {
                    Text(p.name).foregroundStyle(look.dim)
                    ForEach(Array(p.windows.enumerated()), id: \.offset) { _, w in
                        item(w, label: w.window, gauge: gauge, reset: reset)
                    }
                }
            }
        }
        .fixedSize()
    }

    /// Inline but no room for all: one window, clicked through locally.
    @State private var local = 0
    private var single: some View {
        let w = windows[local % windows.count]
        return Button { local = (local + 1) % windows.count } label: {
            HStack(spacing: 4) {
                item(w, label: "\(w.provider) \(w.window)", gauge: false, reset: false)
                if windows.count > 1 { Text("\(local % windows.count + 1)/\(windows.count)").foregroundStyle(look.dim) }
            }
        }
        .buttonStyle(.plain)
        .fixedSize()
    }

    private func item(_ w: L0UsageWindow, label: String, gauge: Bool, reset: Bool) -> some View {
        let color = look.usage(w.pct)
        return HStack(spacing: 4) {
            Text(label).foregroundStyle(look.subtext)
            if gauge {
                ZStack(alignment: .leading) {
                    Capsule().fill(look.rule)
                    Capsule().fill(color).frame(width: gaugeWidth * min(max(w.pct, 0), 100) / 100)
                }
                .frame(width: gaugeWidth, height: max(4, look.lineHeight * 0.32))
            }
            // A cached value is marked `~`, on its label in the carousel, else on the number.
            Text("\(w.stale && !label.hasPrefix("~") ? "~" : "")\(Int(w.pct.rounded()))%").foregroundStyle(color)
            if reset, let r = w.resetLabel { Text("⟳ \(r)").foregroundStyle(look.dim) }
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

extension L0UsageWindow {
    /// The reset as comux formats it for the user's `usage_reset` (nil when off), else — an
    /// older comux — a countdown.
    var resetLabel: String? {
        if let t = reset_text { return t.isEmpty ? nil : t }
        return resets_at.map { untilLabel($0) }
    }
}
