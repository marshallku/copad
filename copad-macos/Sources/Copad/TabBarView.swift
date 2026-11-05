import AppKit

// MARK: - Add Panel Enums

enum AddPanelType {
    case terminal
    case webview
    /// Plugin-provided panel; `panelName` is the `[[panels]]` entry's `name`.
    case plugin(name: String, panelName: String, label: String, icon: String?)
}

enum AddPanelMode {
    case tab
    case splitH
    case splitV
}

enum TabPanelType {
    case terminal
    case webview

    var symbolName: String {
        switch self {
        case .terminal: "terminal"
        case .webview: "globe"
        }
    }
}

// MARK: - TabButton

private final class TabButton: NSView, NSTextFieldDelegate {
    var onSelect: (() -> Void)?
    var onClose: (() -> Void)?
    var onRename: ((String) -> Void)?

    private let titleLabel = NSTextField(labelWithString: "")
    private let closeBtn = NSButton()
    private let iconView = NSImageView()
    private var trackingArea: NSTrackingArea?
    private let theme: CopadTheme

    private(set) var isActive = false
    private var isHovered = false
    private var panelType: TabPanelType = .terminal
    private var isCollapsedState = false

    private var editingField: NSTextField?
    /// Guards against double-commit when both doCommandBy and controlTextDidEndEditing fire.
    private var isCommitting = false

    var isEditing: Bool {
        editingField != nil
    }

    var title: String {
        get { titleLabel.stringValue }
        set {
            titleLabel.stringValue = newValue
            toolTip = newValue
        }
    }

    init(theme: CopadTheme) {
        self.theme = theme
        super.init(frame: .zero)
        setupView()
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    private func setupView() {
        wantsLayer = true
        layer?.cornerRadius = 4

        iconView.translatesAutoresizingMaskIntoConstraints = false
        iconView.isHidden = true
        addSubview(iconView)

        titleLabel.font = NSFont.systemFont(ofSize: 12)
        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.translatesAutoresizingMaskIntoConstraints = false
        addSubview(titleLabel)

        closeBtn.title = "×"
        closeBtn.isBordered = false
        closeBtn.font = NSFont.systemFont(ofSize: 14, weight: .light)
        closeBtn.target = self
        closeBtn.action = #selector(closeTapped)
        closeBtn.translatesAutoresizingMaskIntoConstraints = false
        closeBtn.setContentHuggingPriority(.required, for: .horizontal)
        addSubview(closeBtn)

        NSLayoutConstraint.activate([
            iconView.centerXAnchor.constraint(equalTo: centerXAnchor),
            iconView.centerYAnchor.constraint(equalTo: centerYAnchor),
            iconView.widthAnchor.constraint(equalToConstant: 14),
            iconView.heightAnchor.constraint(equalToConstant: 14),

            closeBtn.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            closeBtn.centerYAnchor.constraint(equalTo: centerYAnchor),
            closeBtn.widthAnchor.constraint(equalToConstant: 16),
            closeBtn.heightAnchor.constraint(equalToConstant: 16),

            titleLabel.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            titleLabel.centerYAnchor.constraint(equalTo: centerYAnchor),
            titleLabel.trailingAnchor.constraint(equalTo: closeBtn.leadingAnchor, constant: -4),
        ])

        let doubleClick = NSClickGestureRecognizer(target: self, action: #selector(doubleClickTapped))
        doubleClick.numberOfClicksRequired = 2
        addGestureRecognizer(doubleClick)

        // AppKit gesture recognizers have no require(toFail:). Double-clicking an active tab:
        // selectTapped fires first but switchTab() is a no-op (same-index guard), so no
        // refreshTabBar() happens and the button survives for the double-click to call
        // beginEditing(). Double-clicking an inactive tab selects it (rebuilding buttons) and
        // does not rename.
        let click = NSClickGestureRecognizer(target: self, action: #selector(selectTapped))
        click.numberOfClicksRequired = 1
        addGestureRecognizer(click)

        applyStyle()
    }

    func setActive(_ active: Bool) {
        isActive = active
        applyStyle()
    }

    func configure(type: TabPanelType, collapsed: Bool) {
        panelType = type
        let img = NSImage(systemSymbolName: type.symbolName, accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 12, weight: .regular))
        iconView.image = img
        setCollapsed(collapsed)
    }

    func setCollapsed(_ collapsed: Bool) {
        isCollapsedState = collapsed
        iconView.isHidden = !collapsed
        titleLabel.isHidden = collapsed
        closeBtn.isHidden = collapsed
    }

    // MARK: - Inline Rename

    private func beginEditing() {
        guard !isCollapsedState, editingField == nil else { return }
        isCommitting = false

        let field = NSTextField()
        field.stringValue = titleLabel.stringValue
        field.font = NSFont.systemFont(ofSize: 12)
        field.isBordered = false
        field.isBezeled = false
        field.focusRingType = .none
        field.backgroundColor = .clear
        field.textColor = theme.text.nsColor
        field.delegate = self
        field.translatesAutoresizingMaskIntoConstraints = false
        titleLabel.isHidden = true
        closeBtn.isHidden = true
        addSubview(field)
        NSLayoutConstraint.activate([
            field.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            field.centerYAnchor.constraint(equalTo: centerYAnchor),
            field.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
        ])
        editingField = field
        field.selectText(nil)
    }

    private func commitEdit() {
        guard let field = editingField, !isCommitting else { return }
        isCommitting = true
        let newTitle = field.stringValue.trimmingCharacters(in: .whitespaces)
        finishEditing()
        if !newTitle.isEmpty {
            onRename?(newTitle)
        }
    }

    private func cancelEdit() {
        guard editingField != nil else { return }
        isCommitting = true
        finishEditing()
    }

    private func finishEditing() {
        guard let field = editingField else { return }
        field.removeFromSuperview()
        titleLabel.isHidden = isCollapsedState
        closeBtn.isHidden = isCollapsedState
        // Nil editingField AFTER makeFirstResponder so that if resign triggers
        // controlTextDidEndEditing synchronously, the identity guard (=== editingField)
        // can still distinguish the right field instead of matching nil === nil.
        window?.makeFirstResponder(nil)
        editingField = nil
    }

    // MARK: - NSTextFieldDelegate

    func control(_ control: NSControl, textView _: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        guard control === editingField else { return false }
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            commitEdit()
            return true
        }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            cancelEdit()
            return true
        }
        return false
    }

    func controlTextDidEndEditing(_ notification: Notification) {
        guard notification.object as? NSTextField === editingField else { return }
        commitEdit()
    }

    private func applyStyle() {
        let bgColor: NSColor
        let textColor: NSColor
        let closeTint: NSColor
        let iconTint: NSColor

        if isActive {
            bgColor = theme.surface2.nsColor
            textColor = theme.text.nsColor
            closeTint = theme.subtext0.nsColor
            iconTint = theme.text.nsColor
        } else if isHovered {
            bgColor = theme.surface1.nsColor
            textColor = theme.subtext1.nsColor
            closeTint = theme.subtext0.nsColor
            iconTint = theme.subtext1.nsColor
        } else {
            bgColor = .clear
            textColor = theme.subtext0.nsColor
            closeTint = .clear
            iconTint = theme.subtext0.nsColor
        }

        layer?.backgroundColor = bgColor.cgColor
        titleLabel.textColor = textColor
        closeBtn.contentTintColor = closeTint
        iconView.contentTintColor = iconTint
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let old = trackingArea { removeTrackingArea(old) }
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .activeInActiveApp],
            owner: self,
            userInfo: nil,
        )
        addTrackingArea(area)
        trackingArea = area
    }

    override func mouseEntered(with _: NSEvent) {
        isHovered = true
        applyStyle()
    }

    override func mouseExited(with _: NSEvent) {
        isHovered = false
        applyStyle()
    }

    @objc private func selectTapped() {
        onSelect?()
    }

    @objc private func doubleClickTapped() {
        beginEditing()
    }

    @objc private func closeTapped() {
        onClose?()
    }
}

// MARK: - AddPanelPopover

private final class AddPanelPopoverController: NSViewController {
    var onSelect: ((AddPanelType, AddPanelMode) -> Void)?
    private let theme: CopadTheme

    init(theme: CopadTheme) {
        self.theme = theme
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    override func loadView() {
        let container = NSView()
        container.wantsLayer = true

        let stack = NSStackView()
        stack.orientation = .vertical
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(stack)

        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: container.topAnchor, constant: 12),
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 12),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -12),
            stack.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -12),
        ])

        let header = makeHeaderRow()
        stack.addArrangedSubview(header)

        let divider = NSBox()
        divider.boxType = .separator
        stack.addArrangedSubview(divider)

        stack.addArrangedSubview(makeRow(type: .terminal, icon: "terminal", label: "Terminal"))
        stack.addArrangedSubview(makeRow(type: .webview, icon: "globe", label: "Browser"))

        // Only manifests declaring `[[panels]]` show up (service-only plugins stay out), sorted
        // by plugin name so row order is stable.
        let pluginRows = pluginPanelRows()
        if !pluginRows.isEmpty {
            let pluginDivider = NSBox()
            pluginDivider.boxType = .separator
            stack.addArrangedSubview(pluginDivider)
            for row in pluginRows {
                stack.addArrangedSubview(row)
            }
        }

        view = container
    }

    /// One row per discovered plugin panel; the default SF Symbol `puzzlepiece.extension` is
    /// used when `[[panels]].icon` is unset.
    private func pluginPanelRows() -> [NSView] {
        var rows: [NSView] = []
        for loaded in PluginManifestStore.discover() {
            let pluginName = loaded.manifest.plugin.name
            for panelDef in loaded.manifest.panels {
                let icon = panelDef.icon ?? "puzzlepiece.extension"
                let label = panelDef.title
                let type: AddPanelType = .plugin(
                    name: pluginName,
                    panelName: panelDef.name,
                    label: label,
                    icon: icon,
                )
                rows.append(makeRow(type: type, icon: icon, label: label))
            }
        }
        return rows
    }

    private func makeHeaderRow() -> NSView {
        let row = NSStackView()
        row.orientation = .horizontal
        row.spacing = 6

        let spacer = NSView()
        spacer.translatesAutoresizingMaskIntoConstraints = false
        spacer.widthAnchor.constraint(equalToConstant: 100).isActive = true
        row.addArrangedSubview(spacer)

        for (sfName, tip) in [
            ("plus.square", "New Tab"),
            ("rectangle.split.2x1", "Split Right"),
            ("rectangle.split.1x2", "Split Down"),
        ] {
            let img = NSImageView(image: symbol(sfName, size: 12))
            img.toolTip = tip
            img.translatesAutoresizingMaskIntoConstraints = false
            img.widthAnchor.constraint(equalToConstant: 28).isActive = true
            img.contentTintColor = theme.subtext0.nsColor
            row.addArrangedSubview(img)
        }

        return row
    }

    private func makeRow(type: AddPanelType, icon: String, label: String) -> NSView {
        let row = NSStackView()
        row.orientation = .horizontal
        row.spacing = 6

        let labelStack = NSStackView()
        labelStack.orientation = .horizontal
        labelStack.spacing = 6
        let iconView = NSImageView(image: symbol(icon, size: 14))
        iconView.contentTintColor = theme.text.nsColor
        iconView.translatesAutoresizingMaskIntoConstraints = false
        iconView.widthAnchor.constraint(equalToConstant: 16).isActive = true
        let labelField = NSTextField(labelWithString: label)
        labelField.font = NSFont.systemFont(ofSize: 13)
        labelField.textColor = theme.text.nsColor
        labelStack.addArrangedSubview(iconView)
        labelStack.addArrangedSubview(labelField)
        labelStack.translatesAutoresizingMaskIntoConstraints = false
        labelStack.widthAnchor.constraint(equalToConstant: 100).isActive = true
        row.addArrangedSubview(labelStack)

        for (btnSymbol, mode) in [
            ("plus.square", AddPanelMode.tab),
            ("rectangle.split.2x1", .splitH),
            ("rectangle.split.1x2", .splitV),
        ] as [(String, AddPanelMode)] {
            let btn = NSButton()
            btn.image = symbol(btnSymbol, size: 13)
            btn.isBordered = false
            btn.contentTintColor = theme.subtext1.nsColor
            btn.translatesAutoresizingMaskIntoConstraints = false
            btn.widthAnchor.constraint(equalToConstant: 28).isActive = true
            btn.heightAnchor.constraint(equalToConstant: 24).isActive = true
            let t = type, m = mode
            btn.target = self
            btn.action = #selector(dummyAction)
            let gr = NSClickGestureRecognizer(target: self, action: #selector(dummyAction))
            gr.numberOfClicksRequired = 1
            btn.addGestureRecognizer(gr)
            addAction(btn, type: t, mode: m)
            row.addArrangedSubview(btn)
        }

        return row
    }

    private var actionMap: [Int: (AddPanelType, AddPanelMode)] = [:]
    private var tagCounter = 0

    private func addAction(_ btn: NSButton, type: AddPanelType, mode: AddPanelMode) {
        tagCounter += 1
        btn.tag = tagCounter
        actionMap[tagCounter] = (type, mode)
        btn.target = self
        btn.action = #selector(panelButtonTapped(_:))
        btn.gestureRecognizers.forEach { btn.removeGestureRecognizer($0) }
    }

    @objc private func dummyAction() {}

    @objc private func panelButtonTapped(_ sender: NSButton) {
        guard let (type, mode) = actionMap[sender.tag] else { return }
        onSelect?(type, mode)
    }

    private func symbol(_ name: String, size: CGFloat) -> NSImage {
        NSImage(systemSymbolName: name, accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: size, weight: .regular))
            ?? NSImage()
    }
}

// MARK: - TabBarView

final class TabBarView: NSView {
    static let height: CGFloat = 36
    static let collapsedTabWidth: CGFloat = 36
    static let collapsedBarWidth: CGFloat = 44

    var onSelectTab: ((Int) -> Void)?
    var onCloseTab: ((Int) -> Void)?
    var onNewPanel: ((AddPanelType, AddPanelMode) -> Void)?
    var onToggle: (() -> Void)?
    var onRenameTab: ((Int, String) -> Void)?

    private(set) var isCollapsed: Bool = false

    private let toggleButton = NSButton()
    private let stackView = NSStackView()
    private let scrollView = NSScrollView()
    private let addButton = NSButton()
    private var tabButtons: [TabButton] = []
    private var theme: CopadTheme
    private let isVertical: Bool
    /// For a vertical bar, the edge the separator sits on: leading for `right`-position tabs,
    /// trailing for `left`-position tabs.
    private let separatorLeading: Bool
    private var popover: NSPopover?
    /// `[window] opacity`, applied only to the bar bg: pills stay opaque so active and hover
    /// states read against any backdrop.
    private var windowOpacity: Double

    private var tabWidthConstraints: [NSLayoutConstraint] = []

    init(theme: CopadTheme, windowOpacity: Double = 1.0, position: TabsPosition = .top) {
        self.theme = theme
        self.windowOpacity = windowOpacity
        self.isVertical = position.isVertical
        self.separatorLeading = position == .right
        super.init(frame: .zero)
        setupView()
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    private func setupView() {
        wantsLayer = true
        layer?.backgroundColor = Self.barBg(theme: theme, opacity: windowOpacity)

        let border = NSView()
        border.wantsLayer = true
        border.layer?.backgroundColor = theme.overlay0.nsColor.cgColor
        border.translatesAutoresizingMaskIntoConstraints = false
        addSubview(border)
        if isVertical {
            let separatorEdge = separatorLeading
                ? border.leadingAnchor.constraint(equalTo: leadingAnchor)
                : border.trailingAnchor.constraint(equalTo: trailingAnchor)
            NSLayoutConstraint.activate([
                border.topAnchor.constraint(equalTo: topAnchor),
                border.bottomAnchor.constraint(equalTo: bottomAnchor),
                separatorEdge,
                border.widthAnchor.constraint(equalToConstant: 1),
            ])
        } else {
            NSLayoutConstraint.activate([
                border.leadingAnchor.constraint(equalTo: leadingAnchor),
                border.trailingAnchor.constraint(equalTo: trailingAnchor),
                border.bottomAnchor.constraint(equalTo: bottomAnchor),
                border.heightAnchor.constraint(equalToConstant: 1),
            ])
        }

        let toggleImg = NSImage(systemSymbolName: "sidebar.left", accessibilityDescription: "Toggle tab bar")?
            .withSymbolConfiguration(.init(pointSize: 12, weight: .regular))
        toggleButton.image = toggleImg
        toggleButton.title = ""
        toggleButton.isBordered = false
        toggleButton.contentTintColor = theme.subtext0.nsColor
        toggleButton.target = self
        toggleButton.action = #selector(toggleTapped)
        toggleButton.toolTip = "Toggle Tab Bar (⌘⇧B)"
        toggleButton.translatesAutoresizingMaskIntoConstraints = false
        addSubview(toggleButton)

        stackView.orientation = isVertical ? .vertical : .horizontal
        stackView.spacing = 2
        stackView.edgeInsets = NSEdgeInsets(top: 4, left: 4, bottom: 4, right: 4)
        stackView.translatesAutoresizingMaskIntoConstraints = false

        scrollView.documentView = stackView
        scrollView.hasHorizontalScroller = false
        scrollView.hasVerticalScroller = false
        scrollView.drawsBackground = false
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        addSubview(scrollView)

        if let img = NSImage(systemSymbolName: "plus", accessibilityDescription: "New panel") {
            addButton.image = img
            addButton.title = ""
        } else {
            addButton.title = "+"
        }
        addButton.isBordered = false
        addButton.contentTintColor = theme.subtext0.nsColor
        addButton.target = self
        addButton.action = #selector(addTabTapped)
        addButton.translatesAutoresizingMaskIntoConstraints = false
        addSubview(addButton)

        if isVertical {
            NSLayoutConstraint.activate([
                toggleButton.topAnchor.constraint(equalTo: topAnchor, constant: 6),
                toggleButton.centerXAnchor.constraint(equalTo: centerXAnchor),
                toggleButton.heightAnchor.constraint(equalToConstant: 24),

                scrollView.topAnchor.constraint(equalTo: toggleButton.bottomAnchor, constant: 6),
                separatorLeading
                    ? scrollView.leadingAnchor.constraint(equalTo: border.trailingAnchor)
                    : scrollView.leadingAnchor.constraint(equalTo: leadingAnchor),
                separatorLeading
                    ? scrollView.trailingAnchor.constraint(equalTo: trailingAnchor)
                    : scrollView.trailingAnchor.constraint(equalTo: border.leadingAnchor),
                scrollView.bottomAnchor.constraint(equalTo: addButton.topAnchor, constant: -4),

                addButton.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -8),
                addButton.centerXAnchor.constraint(equalTo: centerXAnchor),
                addButton.heightAnchor.constraint(equalToConstant: 28),
            ])
            stackView.widthAnchor.constraint(equalTo: scrollView.widthAnchor).isActive = true
            stackView.heightAnchor.constraint(greaterThanOrEqualTo: scrollView.heightAnchor).isActive = true
        } else {
            NSLayoutConstraint.activate([
                toggleButton.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 6),
                toggleButton.centerYAnchor.constraint(equalTo: centerYAnchor),
                toggleButton.widthAnchor.constraint(equalToConstant: 24),

                scrollView.leadingAnchor.constraint(equalTo: toggleButton.trailingAnchor, constant: 8),
                scrollView.topAnchor.constraint(equalTo: topAnchor),
                scrollView.bottomAnchor.constraint(equalTo: border.topAnchor),
                scrollView.trailingAnchor.constraint(equalTo: addButton.leadingAnchor, constant: -4),

                addButton.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
                addButton.centerYAnchor.constraint(equalTo: centerYAnchor),
                addButton.widthAnchor.constraint(equalToConstant: 28),
            ])

            stackView.widthAnchor.constraint(greaterThanOrEqualTo: scrollView.widthAnchor).isActive = true
        }
    }

    // MARK: - Public API

    func setTabs(titles: [String], types: [TabPanelType], activeIndex: Int) {
        // Recreating buttons mid-rename would destroy the in-flight editor; the bar resyncs on
        // the next refresh.
        if tabButtons.contains(where: \.isEditing) { return }

        tabButtons.forEach { $0.removeFromSuperview() }
        tabButtons.removeAll()
        tabWidthConstraints.forEach { $0.isActive = false }
        tabWidthConstraints.removeAll()
        stackView.arrangedSubviews.forEach { stackView.removeArrangedSubview($0); $0.removeFromSuperview() }

        for (i, title) in titles.enumerated() {
            let type = i < types.count ? types[i] : .terminal
            let btn = makeTabButton(index: i, title: title, type: type, active: i == activeIndex)
            stackView.addArrangedSubview(btn)
            tabButtons.append(btn)

            let h = TabBarView.height - 8
            if isVertical {
                // The whole bar narrows to icon-only via its width constraint when collapsed, so
                // no per-tab width swap is needed.
                let w = btn.widthAnchor.constraint(equalTo: stackView.widthAnchor, constant: -8)
                w.isActive = true
                tabWidthConstraints.append(w)
                btn.heightAnchor.constraint(equalToConstant: h).isActive = true
            } else if isCollapsed {
                let w = btn.widthAnchor.constraint(equalToConstant: TabBarView.collapsedTabWidth)
                w.isActive = true
                tabWidthConstraints.append(w)
                btn.heightAnchor.constraint(equalToConstant: h).isActive = true
            } else {
                let minW = btn.widthAnchor.constraint(greaterThanOrEqualToConstant: 80)
                let maxW = btn.widthAnchor.constraint(lessThanOrEqualToConstant: 200)
                minW.isActive = true
                maxW.isActive = true
                tabWidthConstraints.append(contentsOf: [minW, maxW])
                btn.heightAnchor.constraint(equalToConstant: h).isActive = true
            }
        }
    }

    func updateTitle(_ title: String, at index: Int) {
        guard index < tabButtons.count else { return }
        tabButtons[index].title = title
    }

    func setCollapsed(_ collapsed: Bool) {
        isCollapsed = collapsed
        let tint = collapsed ? theme.text.nsColor : theme.subtext0.nsColor
        toggleButton.contentTintColor = tint
    }

    // MARK: - Private

    private func makeTabButton(index: Int, title: String, type: TabPanelType, active: Bool) -> TabButton {
        let btn = TabButton(theme: theme)
        btn.title = title
        btn.setActive(active)
        btn.configure(type: type, collapsed: isCollapsed)
        btn.onSelect = { [weak self] in self?.onSelectTab?(index) }
        btn.onClose = { [weak self] in self?.onCloseTab?(index) }
        btn.onRename = { [weak self] newTitle in self?.onRenameTab?(index, newTitle) }
        return btn
    }

    @objc private func toggleTapped() {
        onToggle?()
    }

    @objc private func addTabTapped() {
        if let existing = popover, existing.isShown {
            existing.close()
            return
        }

        let popoverVC = AddPanelPopoverController(theme: theme)
        popoverVC.onSelect = { [weak self] type, mode in
            self?.popover?.close()
            self?.onNewPanel?(type, mode)
        }

        let pop = NSPopover()
        pop.contentViewController = popoverVC
        pop.behavior = .transient
        pop.appearance = NSAppearance(named: .darkAqua)
        popover = pop

        pop.show(relativeTo: addButton.bounds, of: addButton, preferredEdge: .maxY)
    }

    func applyWindowOpacity(_ opacity: Double, theme: CopadTheme) {
        self.theme = theme
        windowOpacity = opacity
        layer?.backgroundColor = Self.barBg(theme: theme, opacity: opacity)
    }

    private static func barBg(theme: CopadTheme, opacity: Double) -> CGColor {
        opacity < 1.0
            ? theme.surface0.nsColor.withAlphaComponent(CGFloat(opacity)).cgColor
            : theme.surface0.nsColor.cgColor
    }
}
