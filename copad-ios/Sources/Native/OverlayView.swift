import SwiftUI

/// A comux overlay drawn natively (decisions #170) — the `Ctrl-f` switcher, the `Ctrl-b a`
/// notification center, a rename/new-session prompt, a kill confirm. Shared by the iOS app and
/// copad on macOS. Filtering happens here, so it is instant even when comux is far away; only
/// opening and the final answer cross the network.
struct ComuxOverlayView: View {
    let overlay: L0Overlay
    /// (choice, text): both nil = cancelled.
    let answer: (String?, String?) -> Void

    var body: some View {
        switch overlay.spec {
        case let .picker(title, tabs, tab, actions, selected):
            PickerOverlay(title: title, tabs: tabs, initialTab: tab, actions: actions,
                          initialSelection: selected, answer: answer)
        case let .prompt(title, initial, placeholder):
            PromptOverlay(title: title, initial: initial, placeholder: placeholder, answer: answer)
        case let .confirm(message):
            VStack(spacing: 14) {
                Text(message).font(.headline).multilineTextAlignment(.center)
                HStack {
                    Button("Cancel") { answer(nil, nil) }
                        .keyboardShortcut(.cancelAction)
                        .accessibilityIdentifier("overlayCancel")
                    Button("Confirm", role: .destructive) { answer("yes", nil) }
                        .keyboardShortcut(.defaultAction)
                        .accessibilityIdentifier("overlayConfirm")
                }
            }
            .padding(24)
            .frame(minWidth: 320)
        }
    }
}

private struct PromptOverlay: View {
    let title: String
    let initial: String
    let placeholder: String
    let answer: (String?, String?) -> Void
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.headline)
            TextField(placeholder.isEmpty ? title : placeholder, text: $text)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit { answer(nil, text) }
                .accessibilityIdentifier("overlayText")
            HStack {
                Spacer()
                Button("Cancel") { answer(nil, nil) }.keyboardShortcut(.cancelAction)
                Button("OK") { answer(nil, text) }.keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("overlayOK")
            }
        }
        .padding(20)
        .frame(minWidth: 340)
        .onAppear { text = initial; focused = true }
    }
}

private struct PickerOverlay: View {
    let title: String
    let tabs: [L0PickerTab]
    let initialTab: Int
    let actions: [L0PickerAction]
    let initialSelection: String?
    let answer: (String?, String?) -> Void
    @State private var tab = 0
    @State private var filter = ""
    /// The highlighted row: Enter picks it, ↑/↓ move it.
    @State private var sel: String?
    @FocusState private var focused: Bool

    private var rows: [L0PickerRow] {
        guard tabs.indices.contains(tab) else { return [] }
        let all = tabs[tab].rows
        guard !filter.isEmpty else { return all }
        let f = filter.lowercased()
        // Contiguous matches first, then subsequence matches (fzf-ish, like comux's own).
        let contig = all.filter { $0.label.lowercased().contains(f) }
        let subseq = all.filter { r in
            !contig.contains(r) && isSubsequence(f, of: r.label.lowercased())
        }
        return contig + subseq
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(title).font(.headline)
                Spacer()
                Button("Close") { answer(nil, nil) }.keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("overlayCancel")
            }
            if tabs.count > 1 {
                Picker("", selection: $tab) {
                    ForEach(tabs.indices, id: \.self) { i in Text(tabs[i].name).tag(i) }
                }
                .pickerStyle(.segmented)
            }
            TextField("Filter", text: $filter)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit { if let id = current { answer(id, nil) } }
                .onKeyPress(.downArrow) { move(1); return .handled }
                .onKeyPress(.upArrow) { move(-1); return .handled }
                .onChange(of: filter) { _, _ in if !rows.contains(where: { $0.id == sel }) { sel = rows.first?.id } }
                .accessibilityIdentifier("overlayFilter")
            List(rows) { r in
                Button { answer(r.id, nil) } label: {
                    HStack {
                        if r.current == true { Circle().fill(Color.accentColor).frame(width: 6, height: 6) }
                        if r.id == current { Image(systemName: "chevron.right").font(.caption) }
                        VStack(alignment: .leading, spacing: 1) {
                            Text(r.label).lineLimit(1)
                            if let d = r.detail, !d.isEmpty {
                                Text(d).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                            }
                        }
                        Spacer()
                        if let b = r.badge, !b.isEmpty {
                            Text(b).font(.caption.monospaced()).foregroundStyle(badgeColor(b))
                        }
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .listRowBackground(r.id == current ? Color.accentColor.opacity(0.25) : Color.clear)
                .contextMenu {
                    ForEach(actions, id: \.id) { a in
                        Button(a.label) { answer("\(a.id):\(r.id)", nil) }
                    }
                }
                .accessibilityIdentifier("overlayRow-\(r.id)")
            }
            .frame(minHeight: 240)
        }
        .padding(16)
        .frame(minWidth: 420, minHeight: 360)
        .onAppear {
            tab = min(max(initialTab, 0), max(tabs.count - 1, 0))
            sel = initialSelection ?? rows.first?.id
            focused = true
        }
    }

    /// The row Enter picks: the highlighted one if it is still listed, else the best match.
    private var current: String? {
        rows.contains(where: { $0.id == sel }) ? sel : rows.first?.id
    }

    private func move(_ d: Int) {
        let r = rows
        guard !r.isEmpty else { return }
        let i = r.firstIndex(where: { $0.id == current }) ?? 0
        sel = r[(i + d + r.count) % r.count].id
    }

    private func badgeColor(_ b: String) -> Color {
        switch b {
        case "blocked": return .orange
        case "working": return .yellow
        case "ready", "done": return .green
        default: return .secondary
        }
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
