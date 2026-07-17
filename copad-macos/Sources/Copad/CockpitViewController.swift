import AppKit
import CopadCore

/// The agent cockpit panel: a native list of terminal panes with their AI-agent
/// status, attention-sorted, click-to-focus. A view/observer over the
/// app-lifetime `AgentCockpitModel` — it owns no event subscription (the app
/// pump does), just an observer token it releases when the panel goes away.
/// See `docs/agent-cockpit.md`.
@MainActor
final class CockpitViewController: NSViewController, CopadPanel {
    let panelID: String = UUID().uuidString
    var currentTitle: String { "Agents" }

    private let model: AgentCockpitModel
    private weak var tabVC: TabViewController?

    private var tableView: NSTableView!
    private var observerToken: Int?
    private var titleObserver: NSObjectProtocol?
    private struct Row {
        let panelID: String
        let title: String
        let cwd: String
        let state: AgentState
    }
    private var rows: [Row] = []

    init(model: AgentCockpitModel, tabVC: TabViewController) {
        self.model = model
        self.tabVC = tabVC
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) { fatalError() }

    override func loadView() {
        let root = NSView()

        // Header: title + Refresh / Reset.
        let titleLabel = NSTextField(labelWithString: "Agent cockpit")
        titleLabel.font = .boldSystemFont(ofSize: 13)
        let refresh = NSButton(title: "Refresh", target: self, action: #selector(refreshTapped))
        let reset = NSButton(title: "Reset", target: self, action: #selector(resetTapped))
        for b in [refresh, reset] { b.bezelStyle = .rounded; b.controlSize = .small }
        let header = NSStackView(views: [titleLabel, NSView(), refresh, reset])
        header.orientation = .horizontal
        header.spacing = 8
        header.edgeInsets = NSEdgeInsets(top: 6, left: 10, bottom: 6, right: 10)
        header.translatesAutoresizingMaskIntoConstraints = false

        // Table.
        let table = NSTableView()
        table.headerView = nil
        table.rowHeight = 40
        table.backgroundColor = .clear
        table.style = .inset
        let col = NSTableColumn(identifier: .init("agent"))
        col.resizingMask = .autoresizingMask
        table.addTableColumn(col)
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.doubleAction = #selector(rowDoubleClicked)
        tableView = table

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.translatesAutoresizingMaskIntoConstraints = false

        root.addSubview(header)
        root.addSubview(scroll)
        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: root.topAnchor),
            header.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            header.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: header.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: root.bottomAnchor),
        ])
        view = root
    }

    func startIfNeeded() {
        guard observerToken == nil else { return }
        observerToken = model.addObserver { [weak self] in self?.reloadRows() }
        // Terminal renames post `.terminalTitleChanged` (a UI notification, not a
        // bus event), so refresh the row titles off it too.
        titleObserver = NotificationCenter.default.addObserver(
            forName: .terminalTitleChanged, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.reloadRows() }
        }
        reloadRows()
    }

    /// Rebuild the row list from the live pane snapshot + the model overlay,
    /// attention-first. Called on any model/pane-lifecycle change.
    private func reloadRows() {
        guard let tabVC else { rows = []; tableView?.reloadData(); return }
        rows = tabVC.terminalPaneSnapshot()
            .map { Row(panelID: $0.panelID, title: $0.title, cwd: $0.cwd, state: model.state($0.panelID)) }
            .sorted { a, b in
                a.state.rank != b.state.rank ? a.state.rank < b.state.rank
                    : a.title.localizedCaseInsensitiveCompare(b.title) == .orderedAscending
            }
        tableView?.reloadData()
    }

    @objc private func refreshTapped() { reloadRows() }

    @objc private func resetTapped() {
        model.reset()
        reloadRows()
    }

    @objc private func rowDoubleClicked() {
        guard let table = tableView else { return }
        let idx = table.clickedRow
        guard idx >= 0, idx < rows.count else { return }
        let id = rows[idx].panelID
        model.acknowledge(id) // acting on it clears its attention
        tabVC?.activatePanel(id: id)
        reloadRows()
    }

    // MARK: - CopadPanel

    override func removeFromParent() {
        if let token = observerToken {
            model.removeObserver(token)
            observerToken = nil
        }
        if let titleObserver {
            NotificationCenter.default.removeObserver(titleObserver)
            self.titleObserver = nil
        }
        super.removeFromParent()
    }

    func applyBackground(path _: String, tint _: Double, opacity _: Double) {}
    func clearBackground() {}
    func setTint(_: Double) {}

    private static func color(for state: AgentState) -> NSColor {
        switch state {
        case .awaiting: .systemOrange
        case .done: .systemGreen
        case .working: .systemBlue
        case .idle: .tertiaryLabelColor
        }
    }
}

extension CockpitViewController: NSTableViewDataSource, NSTableViewDelegate {
    func numberOfRows(in _: NSTableView) -> Int { rows.count }

    func tableView(_: NSTableView, viewFor _: NSTableColumn?, row: Int) -> NSView? {
        let r = rows[row]
        let dot = NSTextField(labelWithString: "●")
        dot.textColor = Self.color(for: r.state)
        dot.font = .systemFont(ofSize: 13)
        let title = NSTextField(labelWithString: r.title)
        title.font = .systemFont(ofSize: 13)
        title.lineBreakMode = .byTruncatingTail
        let sub = NSTextField(labelWithString: "\(r.state.label)\(r.cwd.isEmpty ? "" : " · \(r.cwd)")")
        sub.font = .systemFont(ofSize: 10)
        sub.textColor = .secondaryLabelColor
        sub.lineBreakMode = .byTruncatingMiddle

        let text = NSStackView(views: [title, sub])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 1

        let hstack = NSStackView(views: [dot, text])
        hstack.orientation = .horizontal
        hstack.alignment = .centerY
        hstack.spacing = 8
        hstack.edgeInsets = NSEdgeInsets(top: 2, left: 8, bottom: 2, right: 8)
        return hstack
    }
}
