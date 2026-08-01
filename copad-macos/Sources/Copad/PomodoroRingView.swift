import AppKit
import CopadCore
import Foundation

/// Native circular progress ring for the Pomodoro timer (Phase 2). Rendered
/// in the macOS status bar IN PLACE OF the pomodoro plugin's poll-text
/// `[[modules]]` entry — the visual the user asked for ("동그란 형태", not
/// numbers). Exact time lives in the tooltip.
///
/// Data flow: the Phase-1 plugin emits `pomodoro.state` on every transition
/// (start / pause / resume / reset / phase-complete). This view subscribes to
/// that event and ticks LOCALLY at 1 Hz off the absolute `ends_at_ms`, so the
/// arc depletes smoothly with no per-second IPC. On launch it also fetches
/// `pomodoro.status` once to seed the current state (an already-running timer
/// would otherwise show nothing until the next mutation).
///
/// Interaction: left-click → `pomodoro.toggle`; right-click → an NSMenu of
/// duration presets → `pomodoro.set_durations`.
@MainActor
final class PomodoroRingView: NSView {
    // Last authoritative state (from a `pomodoro.state` event or the initial
    // `pomodoro.status` seed).
    private var phase: String = "idle"
    /// Absolute expiry while running; `nil` when paused or idle.
    private var endsAtMs: UInt64?
    private var paused = false
    /// Frozen remaining used while paused (`ends_at_ms` is null then).
    private var pausedRemainingMs: UInt64 = 0
    /// Full length of the active phase — the progress denominator (0 = idle).
    private var phaseTotalMs: UInt64 = 0
    private var label = ""

    private var theme: CopadTheme
    private weak var daemonClient: DaemonClient?

    private var channel: TypedEventChannel?
    private var tick: DispatchSourceTimer?
    /// Once a live event has been applied, a late initial `pomodoro.status`
    /// response must not clobber it (ordering guard).
    private var appliedEvent = false
    /// Set on teardown; guards main-hop closures that outlive `stop()`.
    private var stopped = false

    private static let diameter: CGFloat = 16
    private static let lineWidth: CGFloat = 3

    init(theme: CopadTheme, daemonClient: DaemonClient, eventBus: EventBus) {
        self.theme = theme
        self.daemonClient = daemonClient
        super.init(frame: NSRect(x: 0, y: 0, width: Self.diameter, height: Self.diameter))
        translatesAutoresizingMaskIntoConstraints = false
        wantsLayer = true
        toolTip = "Pomodoro"
        NSLayoutConstraint.activate([
            widthAnchor.constraint(equalToConstant: Self.diameter),
            heightAnchor.constraint(equalToConstant: Self.diameter),
        ])
        subscribe(to: eventBus)
        seedInitialState()
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) { fatalError() }

    override var intrinsicContentSize: NSSize {
        NSSize(width: Self.diameter, height: Self.diameter)
    }

    // MARK: - Event feed

    private func subscribe(to eventBus: EventBus) {
        let channel = eventBus.subscribeTyped(kinds: ["pomodoro.state"])
        self.channel = channel
        // `receive()` blocks; loop on a background thread and hop each payload
        // to main in arrival order (mirrors AppDelegate's agent-event feed).
        Thread.detachNewThread {
            while let event = channel.receive() {
                let payload = event.data as? [String: Any] ?? [:]
                DispatchQueue.main.async { [weak self] in
                    MainActor.assumeIsolated {
                        self?.apply(payload, fromEvent: true)
                    }
                }
            }
        }
    }

    /// Retry budget: seeding must survive a slow daemon startup ("retry until
    /// connected"), not just the ~first few seconds. 1 s × 120 ≈ 2 minutes —
    /// far longer than any realistic `copadd` boot, while still bounded so a
    /// permanently-absent daemon (plugin uninstalled) can't poll forever. The
    /// loop stops the instant a seed succeeds or a live event arrives, so the
    /// budget is only ever reached when there's genuinely nothing to talk to.
    private nonisolated static let seedAttempts = 120
    private nonisolated static let seedInterval: TimeInterval = 1.0

    /// Seed from `pomodoro.status` so a ring that starts while a timer is
    /// ALREADY running shows the live state immediately (no event fires until
    /// the next transition). Retries because `loadModules` runs before the
    /// daemon connection is up, so the first `forward` returns
    /// `daemon_unavailable`. Stops once seeded or a live event has arrived.
    private func seedInitialState(attemptsLeft: Int = seedAttempts) {
        guard !stopped, !appliedEvent, attemptsLeft > 0 else { return }
        daemonClient?.forward(method: "pomodoro.status", params: [:]) { [weak self] result in
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    guard let self, !self.stopped, !self.appliedEvent else { return }
                    if let payload = result as? [String: Any] {
                        // A valid snapshot — seed and stop retrying.
                        self.apply(payload, fromEvent: false)
                        return
                    }
                    // RPCError / nil (daemon not connected yet) → retry.
                    DispatchQueue.main.asyncAfter(deadline: .now() + Self.seedInterval) {
                        self.seedInitialState(attemptsLeft: attemptsLeft - 1)
                    }
                }
            }
        }
    }

    private func apply(_ p: [String: Any], fromEvent: Bool) {
        if stopped { return }
        if fromEvent { appliedEvent = true }
        phase = p["phase"] as? String ?? "idle"
        endsAtMs = Self.msField(p, "ends_at_ms")
        paused = p["paused"] as? Bool ?? false
        pausedRemainingMs = Self.msField(p, "remaining_ms") ?? 0
        phaseTotalMs = Self.msField(p, "phase_total_ms") ?? 0
        label = p["label"] as? String ?? ""
        restartTick()
        updateTooltip()
        needsDisplay = true
    }

    /// Read a millis field defensively: `NSNumber.uint64Value` WRAPS a
    /// negative (`-1` → `UInt64.max`), which would blow up downstream time
    /// math. Parse through `Double`, rejecting non-finite / negative / absurd
    /// values (a malformed or mixed-version payload) so they degrade to nil →
    /// idle/track-only rather than crash. The cap (~285k years in ms) is far
    /// above any real epoch/remaining value.
    private static func msField(_ p: [String: Any], _ key: String) -> UInt64? {
        guard let n = p[key] as? NSNumber else { return nil }
        let d = n.doubleValue
        guard d.isFinite, d >= 0, d < 9.0e18 else { return nil }
        return UInt64(d)
    }

    // MARK: - Ticking

    private func restartTick() {
        tick?.cancel()
        tick = nil
        // Only a running (non-paused, non-idle) phase needs a per-second
        // redraw; a paused or idle ring is static.
        guard !paused, endsAtMs != nil, phase != "idle" else { return }
        let t = DispatchSource.makeTimerSource(queue: .main)
        t.schedule(deadline: .now() + 1, repeating: 1)
        t.setEventHandler { [weak self] in self?.onTick() }
        t.resume()
        tick = t
    }

    private func onTick() {
        if stopped { return }
        needsDisplay = true
        updateTooltip()
        // Stop churning once we hit zero; the plugin's transition event will
        // move us to the next phase.
        if let ends = endsAtMs, nowMs() >= ends {
            tick?.cancel()
            tick = nil
        }
    }

    private func currentRemainingMs() -> UInt64 {
        if paused { return pausedRemainingMs }
        guard let ends = endsAtMs else { return 0 }
        let now = nowMs()
        return ends > now ? ends - now : 0
    }

    // MARK: - Drawing

    private func phaseColor() -> NSColor {
        switch phase {
        case "work": theme.red.nsColor
        case "break": theme.palette.count > 2 ? theme.palette[2].nsColor : theme.accent.nsColor
        case "long_break": theme.palette.count > 4 ? theme.palette[4].nsColor : theme.accent.nsColor
        default: theme.overlay0.nsColor
        }
    }

    override func draw(_: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let inset = Self.lineWidth / 2 + 0.5
        let rect = bounds.insetBy(dx: inset, dy: inset)
        let center = CGPoint(x: rect.midX, y: rect.midY)
        let radius = min(rect.width, rect.height) / 2
        guard radius > 0 else { return }

        ctx.setLineWidth(Self.lineWidth)
        ctx.setLineCap(.round)

        // Background track (full circle).
        ctx.setStrokeColor(theme.overlay0.nsColor.cgColor)
        ctx.beginPath()
        ctx.addArc(center: center, radius: radius, startAngle: 0, endAngle: .pi * 2, clockwise: false)
        ctx.strokePath()

        // Progress arc: remaining fraction, from 12 o'clock sweeping clockwise.
        let frac = PomodoroProgress.fraction(
            remainingMs: Double(currentRemainingMs()),
            totalMs: Double(phaseTotalMs),
        )
        guard frac > 0 else { return }
        let start = CGFloat.pi / 2 // 12 o'clock (unflipped NSView: +y is up)
        let end = start - CGFloat(frac) * .pi * 2
        let color = paused ? phaseColor().withAlphaComponent(0.4) : phaseColor()
        ctx.setStrokeColor(color.cgColor)
        ctx.beginPath()
        ctx.addArc(center: center, radius: radius, startAngle: start, endAngle: end, clockwise: true)
        ctx.strokePath()
    }

    // MARK: - Interaction

    override func mouseDown(with _: NSEvent) {
        invoke("pomodoro.toggle", params: [:])
    }

    /// Right-click context menu (the app's first — no prior precedent). Built
    /// fresh per click so the toggle label reflects current state.
    override func menu(for _: NSEvent) -> NSMenu? {
        let menu = NSMenu()
        let toggleTitle = (endsAtMs != nil && !paused) ? "Pause" : (paused ? "Resume" : "Start")
        menu.addItem(withActionTitle: toggleTitle, target: self, action: #selector(menuToggle))
        menu.addItem(withActionTitle: "Reset", target: self, action: #selector(menuReset))
        menu.addItem(.separator())
        let header = NSMenuItem(title: "Focus / break (min)", action: nil, keyEquivalent: "")
        header.isEnabled = false
        menu.addItem(header)
        for (work, brk) in [(25, 5), (50, 10), (15, 3), (90, 20)] {
            let item = NSMenuItem(
                title: "\(work) / \(brk)",
                action: #selector(menuPreset(_:)),
                keyEquivalent: "",
            )
            item.target = self
            item.representedObject = [work, brk]
            menu.addItem(item)
        }
        return menu
    }

    @objc private func menuToggle() { invoke("pomodoro.toggle", params: [:]) }
    @objc private func menuReset() { invoke("pomodoro.reset", params: [:]) }

    @objc private func menuPreset(_ sender: NSMenuItem) {
        guard let pair = sender.representedObject as? [Int], pair.count == 2 else { return }
        // Only work/break are sent; long_break_min & rounds_before_long are
        // intentionally left unchanged (the plugin keeps current on absent).
        invoke("pomodoro.set_durations", params: ["work_min": pair[0], "break_min": pair[1]])
    }

    private func invoke(_ method: String, params: [String: Any]) {
        daemonClient?.forward(method: method, params: params) { result in
            if let err = result as? RPCError {
                FileHandle.standardError.write(Data("[copad] \(method) failed: \(err.code) — \(err.message)\n".utf8))
            }
        }
    }

    // MARK: - Theme / lifecycle

    func applyTheme(_ newTheme: CopadTheme) {
        theme = newTheme
        needsDisplay = true
    }

    /// Stop the tick timer and unblock the receive() thread. Idempotent.
    func stop() {
        stopped = true
        tick?.cancel()
        tick = nil
        channel?.close()
        channel = nil
    }

    // MARK: - Helpers

    private func updateTooltip() {
        let name: String
        switch phase {
        case "work": name = "Focus"
        case "break": name = "Break"
        case "long_break": name = "Long break"
        default: name = "Pomodoro — idle"
        }
        var s = name
        if phase != "idle" {
            s += " \(PomodoroProgress.mmss(currentRemainingMs()))"
            if paused { s += " (paused)" }
        }
        if !label.isEmpty { s += " · \(label)" }
        toolTip = s
    }

    private func nowMs() -> UInt64 { UInt64(Date().timeIntervalSince1970 * 1000) }
}

private extension NSMenu {
    func addItem(withActionTitle title: String, target: AnyObject, action: Selector) {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: "")
        item.target = target
        addItem(item)
    }
}
